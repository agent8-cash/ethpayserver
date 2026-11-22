#!/usr/bin/env bash
# Proves check-no-ticket-refs.sh goes red on the cases it claims to.
#
# The guard exists because a rule that nothing checks is not a rule, so
# "it passed on a clean tree" is not evidence that it works.
set -uo pipefail

GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-no-ticket-refs.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
check() { # name expected_rc
  local name="$1" want="$2" got
  ( cd "$TMP" && "$GUARD" ) >/dev/null 2>&1
  got=$?
  if [ "$got" -ne "$want" ]; then
    echo "FAIL: $name - expected exit $want, got $got"
    fail=1
  else
    echo "ok: $name (exit $got)"
  fi
}

cd "$TMP"
git init -q .
git config user.email t@t; git config user.name t
echo "code" > src.rs
git add -A && git commit -qm "base commit"

echo "more" >> src.rs
git add -A && git commit -qm "an ordinary commit"
check "a clean commit passes" 0

echo "// see RCS-1234" >> src.rs
git add -A && git commit -qm "an RCS ID in a file"
check "an RCS ID in a tracked file is refused" 1

git reset -q --hard HEAD~1
check "green again once the file is gone" 0

echo "// see AG8-1234" >> src.rs
git add -A && git commit -qm "an AG8 ID in a file"
check "an AG8 ID in a tracked file is refused" 1

git reset -q --hard HEAD~1
check "green again once the file is gone" 0

echo "// see linear.app/issue/ABC-123" >> src.rs
git add -A && git commit -qm "a tracker link in a file"
check "a tracker link in a tracked file is refused" 1

[ "$fail" -eq 0 ] && echo "check-no-ticket-refs.sh behaves as documented"
exit "$fail"
