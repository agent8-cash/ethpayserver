#!/usr/bin/env bash
# Test for check-advisory-expires.sh
set -uo pipefail

temp_audit=$(mktemp)
trap 'rm -f "$temp_audit"' EXIT

# Test 1: Valid EXPIRES in the future should pass
cat > "$temp_audit" << 'EOF'
[advisories]
ignore = [
    # Some issue. EXPIRES 2099-12-31
    "RUSTSEC-2024-0001",
]
EOF

if AUDIT_FILE="$temp_audit" ./scripts/check-advisory-expires.sh; then
    echo "Test 1 PASS: Future EXPIRES date accepted"
else
    echo "Test 1 FAIL: Future EXPIRES date should be accepted"
    exit 1
fi

# Test 2: Expired EXPIRES should fail
cat > "$temp_audit" << 'EOF'
[advisories]
ignore = [
    # Some issue. EXPIRES 2020-01-01
    "RUSTSEC-2024-0001",
]
EOF

if AUDIT_FILE="$temp_audit" ./scripts/check-advisory-expires.sh; then
    echo "Test 2 FAIL: Expired EXPIRES date should cause failure"
    exit 1
else
    echo "Test 2 PASS: Expired EXPIRES date correctly rejected"
fi

# Test 3: No EXPIRES dates should pass (no requirement to have them)
cat > "$temp_audit" << 'EOF'
[advisories]
ignore = [
    # Some unmaintained issue (no permanent fix)
    "RUSTSEC-2024-0001",
]
EOF

if AUDIT_FILE="$temp_audit" ./scripts/check-advisory-expires.sh; then
    echo "Test 3 PASS: Advisory without EXPIRES date is allowed"
else
    echo "Test 3 FAIL: Advisory without EXPIRES date should pass"
    exit 1
fi

# Test 4: Multiple advisories, one expired
cat > "$temp_audit" << 'EOF'
[advisories]
ignore = [
    # Issue 1. EXPIRES 2099-12-31
    "RUSTSEC-2024-0001",
    # Issue 2. EXPIRES 2020-01-01
    "RUSTSEC-2024-0002",
]
EOF

if AUDIT_FILE="$temp_audit" ./scripts/check-advisory-expires.sh; then
    echo "Test 4 FAIL: One expired advisory should cause failure"
    exit 1
else
    echo "Test 4 PASS: One expired advisory correctly rejected"
fi

echo "All tests passed"
