#!/usr/bin/env bash
# Proves check-test-inventory.sh goes red and green on the cases it claims to,
# against a real throwaway git repository.
set -uo pipefail

GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-test-inventory.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
check() { # name expected_rc [expected output substring]
  local name="$1" want="$2" needle="${3:-}" got out
  out="$( cd "$TMP" && BASE_REF=base "$GUARD" 2>&1 )"
  got=$?
  if [ "$got" -ne "$want" ] || { [ -n "$needle" ] && ! grep -qF -- "$needle" <<<"$out"; }; then
    echo "FAIL: $name - expected exit $want ${needle:+with '$needle'}, got $got"
    echo "$out" | sed 's/^/    /'
    fail=1
  else
    echo "ok: $name (exit $got)"
  fi
}
commit() { git add -A && git commit -qm "$1"; }

cd "$TMP"
git init -q -b main .
git config user.email t@t; git config user.name t
mkdir src
cat > src/alerting.rs <<'RS'
fn helper() {}

#[test]
fn keeps_newer_version() {}

#[tokio::test]
#[ignore]
async fn push_failing_reaches_sentry() {}

#[test]
fn briefly_failing_is_not_an_event() {}
RS
printf '#[test]\nfn unrelated() {}\n' > src/other.rs
commit base
git branch base

git checkout -q -b feature
check "no citation, nothing to compare" 0

echo '// touched' >> src/alerting.rs
commit "work

Cites-Tests: src/alerting.rs"
check "cited file with every test intact passes" 0

# The incident: a merge resolves the file to an older copy.
sed -i '/briefly_failing_is_not_an_event/,$d;' src/alerting.rs
sed -i '$d' src/alerting.rs  # drop the dangling attribute
commit "merge main"
check "silently dropped cited test is refused" 1 "briefly_failing_is_not_an_event"

git commit -q --allow-empty -m "intended removal

Removes-Test: briefly_failing_is_not_an_event"
check "declared removal passes" 0

git reset -q --hard HEAD~2

# Moving a test to another file is not a deletion.
sed -i '/^fn keeps_newer_version/d' src/alerting.rs
sed -i '0,/^#\[test\]$/{//d}' src/alerting.rs  # its now-dangling attribute
printf '#[test]\nfn keeps_newer_version() {}\n' > src/moved.rs
commit "move test to another file"
check "a test moved to another file is not a deletion" 0

# An uncited file losing a test is not this check's business.
printf '// emptied\n' > src/other.rs
commit "gut uncited file"
check "tests lost from an uncited file do not fire" 0

# A multi-line attribute must still be read, in the base and in the head.
git checkout -q base
git checkout -q -b multiline
cat > src/db.rs <<'RS'
#[sqlx::test(
    migrations = "./migrations"
)]
async fn survives_a_multiline_attribute() {}

#[test] fn one_liner_is_seen() {}
RS
commit "base with multiline"
git branch -f base
echo '// touched' >> src/db.rs
commit "work

Cites-Tests: src/db.rs"
check "multi-line attribute test intact passes" 0
: > src/db.rs
commit "gut db.rs"
check "dropped multi-line-attribute test is refused" 1 "survives_a_multiline_attribute"
check "dropped one-line test is refused" 1 "one_liner_is_seen"

git reset -q --hard HEAD~1
git commit -q --allow-empty -m "typo

Cites-Tests: src/dbb.rs"
check "cited path missing everywhere is refused" 1 "neither at the merge base"
git reset -q --hard HEAD~1
git commit -q --allow-empty -m "not rust

Cites-Tests: README.md"
check "cited non-rust path is refused" 1 "not a .rs file"
git reset -q --hard HEAD~1

# A test renamed in place has lost its old name: it must be declared.
sed -i 's/survives_a_multiline_attribute/renamed_in_place/' src/db.rs
commit "rename test"
check "in-place rename is refused until declared" 1 "survives_a_multiline_attribute"
git commit -q --allow-empty -m "declared

Removes-Test: survives_a_multiline_attribute"
check "declared in-place rename passes" 0

[ "$fail" -eq 0 ] && echo "all cases behaved" || exit 1
