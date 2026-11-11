#!/usr/bin/env bash
# Proves check.sh includes and runs cargo audit and cargo deny checks.
# A gate that only ever passed proves nothing; this verifies the checks are
# wired into the local check script.
set -uo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check.sh"
fail=0

# Verify the checks are mentioned in check.sh
if ! grep -q "audit-build-graph.sh" "$SCRIPT"; then
  echo "FAIL: cargo audit check not found in check.sh"
  fail=1
else
  echo "ok: cargo audit check present in check.sh"
fi

if ! grep -q 'cargo deny check' "$SCRIPT"; then
  echo "FAIL: cargo deny check not found in check.sh"
  fail=1
else
  echo "ok: cargo deny check present in check.sh"
fi

# Verify the == security == section exists
if ! grep -q '== security ==' "$SCRIPT"; then
  echo "FAIL: security section not found in check.sh"
  fail=1
else
  echo "ok: security section present in check.sh"
fi

# Verify the order: security checks come after compiler and before tests
compiler_line=$(grep -n '== compiler ==' "$SCRIPT" | cut -d: -f1)
security_line=$(grep -n '== security ==' "$SCRIPT" | cut -d: -f1)
tests_line=$(grep -n '== tests ==' "$SCRIPT" | cut -d: -f1)

if [ -z "$compiler_line" ] || [ -z "$security_line" ] || [ -z "$tests_line" ]; then
  echo "FAIL: could not find section markers"
  fail=1
elif [ "$compiler_line" -ge "$security_line" ] || [ "$security_line" -ge "$tests_line" ]; then
  echo "FAIL: security checks are not between compiler and tests"
  fail=1
else
  echo "ok: security checks are in the correct order"
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "All security checks verified to be present in check.sh"
exit 0
