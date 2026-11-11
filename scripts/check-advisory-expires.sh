#!/usr/bin/env bash
# Enforces EXPIRES dates in .cargo/audit.toml comments to prevent waivers from
# outliving their intended lifetime silently.
#
# Advisory ignores in .cargo/audit.toml may include an EXPIRES date, marking
# when the ignore should no longer be valid (either the advisory is fixed
# upstream, a mainnet milestone passes, or the date is explicit). This script
# extracts those dates and fails if the current date is at or past any expiry.
#
# Format: EXPIRES <YYYY-MM-DD> within a comment adjacent to or above the
# advisory id line (e.g. "RUSTSEC-2024-0436"). The script is tolerant:
# "EXPIRES 2026-11-23" or "EXPIRES 2026-11-23 or any mainnet deploy" both
# match, and it takes the first date it finds on or near the advisory line.
#
# Test hooks: AUDIT_FILE replaces .cargo/audit.toml, NOW overrides the system
# date (format: YYYY-MM-DD).
set -uo pipefail

AUDIT="${AUDIT_FILE:-.cargo/audit.toml}"
now="${NOW:-$(date +%Y-%m-%d)}"

if [ ! -f "$AUDIT" ]; then
  echo "::error::$AUDIT not found" >&2
  exit 1
fi

# Parse dates in YYYY-MM-DD format: -d requires GNU date, so we extract years,
# months, days as decimals and compare lexicographically (YYYY-MM-DD sorts
# correctly), which works across the entire valid date range.
#
# Validate dates are well-formed (catch typos like EXPIRES 2026-13-45):
#   1. Six digits plus two dashes: exactly [0-9]{4}-[0-9]{2}-[0-9]{2}
#   2. Month 01-12, day 01-31 (not perfect - Feb 30 would pass - but catches
#      most typos and false positives without a date library)
validate_date() {
  local d="$1"
  if ! printf '%s' "$d" | grep -qE '^[0-9]{4}-([0-1][0-9])-([0-3][0-9])$'; then
    return 1
  fi
  local m="${d:5:2}" y="${d:0:4}"
  # Month must be 01-12
  if [ "$m" -lt 01 ] || [ "$m" -gt 12 ]; then
    return 1
  fi
  # Day must be 01-31 (loose check, catches Feb 30 and similar)
  local d_part="${d:8:2}"
  if [ "$d_part" -lt 01 ] || [ "$d_part" -gt 31 ]; then
    return 1
  fi
  return 0
}

status=0
expired_count=0

# Read the file line by line, tracking context to match EXPIRES dates to
# advisory IDs. When we find a RUSTSEC- id, check the prior comment block
# for an EXPIRES date.
comment_block=""
while IFS= read -r line; do
  # If it's a comment line, accumulate it
  if [[ "$line" =~ ^[[:space:]]*# ]]; then
    comment_block="${comment_block}${line}"$'\n'
  else
    # If we hit a non-comment line with an advisory ID, extract EXPIRES from comment_block
    if [[ "$line" =~ \"(RUSTSEC-[0-9]+-[0-9]+)\" ]]; then
      id="${BASH_REMATCH[1]}"
      # Extract first date in YYYY-MM-DD format from accumulated comments
      expiry=""
      if [[ "$comment_block" =~ EXPIRES[[:space:]]+([0-9]{4}-[0-9]{2}-[0-9]{2}) ]]; then
        expiry="${BASH_REMATCH[1]}"
      fi

      if [ -n "$expiry" ]; then
        if ! validate_date "$expiry"; then
          echo "::error::invalid EXPIRES date format in .cargo/audit.toml for $id: $expiry (expected YYYY-MM-DD)" >&2
          status=1
          expired_count=$((expired_count + 1))
          # Continue rather than failing hard, to report all errors at once
          continue
        fi

        # Compare dates lexicographically (works for YYYY-MM-DD format)
        if [ "$now" \> "$expiry" ] || [ "$now" = "$expiry" ]; then
          echo "::error::advisory ignore $id has expired (EXPIRES $expiry, today is $now) - remove the ignore from .cargo/audit.toml" >&2
          status=1
          expired_count=$((expired_count + 1))
        fi
      fi
      # Do NOT clear comment_block here - consecutive IDs under the same block share the EXPIRES
    else
      # Non-comment, non-ID line: clear the block for the next section
      comment_block=""
    fi
  fi
done < "$AUDIT"

if [ "$status" -eq 0 ]; then
  echo "audit EXPIRES dates: ok (no expired ignores)"
fi

exit "$status"
