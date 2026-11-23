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
# Test hooks: AUDIT_FILE replaces .cargo/audit.toml
set -uo pipefail

AUDIT="${AUDIT_FILE:-.cargo/audit.toml}"

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
today=$(date +%s)

while IFS=$'\t' read -r id expires_date; do
    if [ -z "$id" ] || [ -z "$expires_date" ]; then
        continue
    fi

    expires_epoch=$(date -d "$expires_date" +%s 2>/dev/null || echo "")

    if [ -z "$expires_epoch" ]; then
        echo "ERROR: $id has unparseable EXPIRES date: $expires_date"
        failed=1
        continue
    fi

    if [ "$today" -gt "$expires_epoch" ]; then
        days_ago=$(( (today - expires_epoch) / 86400 ))
        echo "EXPIRED  $id  expires on $expires_date (expired $days_ago days ago)"
        failed=1
    fi
done < "$temp_parsed"

if [ "$failed" -ne 0 ]; then
    echo "FAIL: at least one advisory ignore has an expired EXPIRES date (see EXPIRED lines above). Either resolve the underlying issue or renew the EXPIRES date."
    exit 1
fi

echo "OK: all advisory ignores with EXPIRES dates are still valid."
exit 0
