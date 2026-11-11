#!/usr/bin/env bash
# Proves check-advisory-expires.sh goes red on the cases it claims to,
# against test .cargo/audit.toml fixtures.
#
# The guard exists to catch dated waivers that have silently outlived their
# intended lifetime, so "it passed on a tree with no expiries" is not evidence
# it works.
set -uo pipefail

GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-advisory-expires.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
check() { # name expected_rc now_date
  local name="$1" want="$2" now="$3" got
  ( AUDIT_FILE="$TMP/audit.toml" NOW="$now" "$GUARD" ) >/dev/null 2>&1
  got=$?
  if [ "$got" -ne "$want" ]; then
    echo "FAIL: $name - expected exit $want, got $got"
    fail=1
  else
    echo "ok: $name (exit $got)"
  fi
}

cd "$TMP"

# Baseline: no EXPIRES dates at all - should pass.
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    "RUSTSEC-2024-0001",
    "RUSTSEC-2024-0002",
]
EOF
check "no EXPIRES dates pass" 0 "2026-11-11"

# Single EXPIRES in the future - should pass.
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # This one expires in the future
    # EXPIRES 2026-11-25
    "RUSTSEC-2024-0001",
]
EOF
check "future EXPIRES passes" 0 "2026-11-11"

# EXPIRES date today (exact match) - should fail (today counts as expired).
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # This expires today
    # EXPIRES 2026-11-11
    "RUSTSEC-2024-0001",
]
EOF
check "EXPIRES today fails" 1 "2026-11-11"

# EXPIRES date in the past - should fail.
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # This expired yesterday
    # EXPIRES 2026-11-10
    "RUSTSEC-2024-0001",
]
EOF
check "past EXPIRES fails" 1 "2026-11-11"

# Mixed: one expired, one future - should fail (reports the expired one).
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # EXPIRES 2026-11-10
    "RUSTSEC-2024-0001",
    # EXPIRES 2026-11-25
    "RUSTSEC-2024-0002",
]
EOF
check "mixed EXPIRES reports expired" 1 "2026-11-11"

# EXPIRES at end of year boundary.
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # EXPIRES 2026-12-31
    "RUSTSEC-2024-0001",
]
EOF
check "year-boundary EXPIRES passes" 0 "2026-12-30"

# Malformed EXPIRES date (not a date, or typo).
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # EXPIRES 2026-13-45
    "RUSTSEC-2024-0001",
]
EOF
check "malformed EXPIRES date is error" 1 "2026-11-11"

# EXPIRES with extra text around it (the actual format used in ethpayserver).
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # EXPIRES 2026-11-23 or any mainnet deploy, whichever is sooner
    "RUSTSEC-2024-0001",
]
EOF
check "EXPIRES with trailing text passes" 0 "2026-11-11"

# Day-of-month boundary (01, 15, 28, 30, 31).
cat > audit.toml << 'EOF'
[advisories]
ignore = [
    # EXPIRES 2026-11-01
    "RUSTSEC-2024-0001",
    # EXPIRES 2026-11-15
    "RUSTSEC-2024-0002",
    # EXPIRES 2026-11-28
    "RUSTSEC-2024-0003",
    # EXPIRES 2026-11-30
    "RUSTSEC-2024-0004",
    # EXPIRES 2026-12-31
    "RUSTSEC-2024-0005",
]
EOF
check "month-boundary dates" 1 "2026-11-15"  # exact match on one date

[ "$fail" -eq 0 ] && echo "check-advisory-expires.sh behaves as documented"
exit "$fail"
