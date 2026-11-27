#!/usr/bin/env bash
# Check that all EXPIRES dates in .cargo/audit.toml advisory ignores are in the
# future. Each advisory ignore may have an EXPIRES date in the comment block
# above it, formatted as "EXPIRES YYYY-MM-DD". If present, the date is checked
# against today. If any EXPIRES date is in the past, the script fails with a
# list of expired advisories.
#
# Advisories without an EXPIRES date are allowed - they are treated as permanent
# waivers. This check only enforces those that explicitly have an expiration.
#
# The date is checked lexicographically, as a YYYY-MM-DD string against
# today's UTC date, exactly like check-advisory-ignores.sh's own EXPIRES
# check: a waiver fails only when today > EXPIRES (today = EXPIRES passes).
# An earlier version compared `date +%s` (current moment, with a
# time-of-day component) against midnight of the EXPIRES date, which made
# every waiver read as already expired for the entire day it was due to
# expire - the exact same-day boundary this script must agree with the
# other check on, not merely resemble.
#
# Test hooks: AUDIT_FILE replaces .cargo/audit.toml. AUDIT_TODAY overrides
# the current date (requires AUDIT_TODAY_OK=1, same guard as
# check-advisory-ignores.sh, so a fixture date can't silently substitute).
set -uo pipefail

AUDIT="${AUDIT_FILE:-.cargo/audit.toml}"

if [ -n "${AUDIT_TODAY:-}" ]; then
  if [ "${AUDIT_TODAY_OK:-}" != 1 ]; then
    echo "ERROR: AUDIT_TODAY is set but AUDIT_TODAY_OK is not 1 - refusing to use a non-system date without explicit consent. COULD NOT RUN (not a pass)." >&2
    exit 2
  fi
  today="$AUDIT_TODAY"
else
  today="$(date -u +%F)"
fi

temp_parsed=$(mktemp)
trap 'rm -f "$temp_parsed"' EXIT

# Parse the audit file to extract advisory IDs and their EXPIRES dates
cat > "$temp_parsed.py" << 'PYTHON_EOF'
import sys
import re

audit_file = sys.argv[1]
with open(audit_file, 'r') as f:
    lines = f.readlines()

in_advisories = False
comment_buffer = ""

for line in lines:
    if '[advisories]' in line:
        in_advisories = True
        comment_buffer = ""
        continue

    if in_advisories and line.strip() == ']':
        in_advisories = False
        comment_buffer = ""
        continue

    if in_advisories:
        # Check if this line has an advisory ID
        id_match = re.search(r'"(RUSTSEC-\d+-\d+)"', line)
        if id_match:
            advisory_id = id_match.group(1)
            # Look for EXPIRES date in the buffer
            expires_match = re.search(r'EXPIRES\s+(\d{4}-\d{2}-\d{2})', comment_buffer)
            if expires_match:
                expires_date = expires_match.group(1)
                print(f"{advisory_id}\t{expires_date}")
            comment_buffer = ""
        else:
            # Accumulate comment lines
            comment_buffer += line

PYTHON_EOF

python3 "$temp_parsed.py" "$AUDIT" > "$temp_parsed" 2>/dev/null || true
rm -f "$temp_parsed.py"

failed=0

while IFS=$'\t' read -r id expires_date; do
    if [ -z "$id" ] || [ -z "$expires_date" ]; then
        continue
    fi

    if ! date -u -d "$expires_date" +%F >/dev/null 2>&1; then
        echo "ERROR: $id has unparseable EXPIRES date: $expires_date"
        failed=1
        continue
    fi

    if [ "$today" \> "$expires_date" ]; then
        echo "EXPIRED  $id  expires on $expires_date (today is $today)"
        failed=1
    fi
done < "$temp_parsed"

if [ "$failed" -ne 0 ]; then
    echo "FAIL: at least one advisory ignore has an expired EXPIRES date (see EXPIRED lines above). Either resolve the underlying issue or renew the EXPIRES date."
    exit 1
fi

echo "OK: all advisory ignores with EXPIRES dates are still valid."
exit 0
