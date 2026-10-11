#!/usr/bin/env bash
# Fails when a test that a change cites as its justification has silently
# disappeared since the merge base.
#
# Deleting a test cannot fail a test run. A routine merge of the base branch
# can resolve a test file in favour of the older copy and drop tests that
# landed an hour earlier - including the very tests the change is justified
# by - and CI stays green throughout. Nobody decides to remove the control; a
# three-way merge picks a side. The only thing that noticed, once, was a human
# reading the diff.
#
# This compares NAMES, not line counts (a line count moves for a hundred
# innocent reasons and would be ignored within a week) and not behaviour (it
# needs no test to run, so it cannot be fooled by a test that passes
# vacuously - a separate problem).
#
# How a change cites its justification: a trailer line in any commit message
# between the merge base and HEAD,
#
#     Cites-Tests: path/to/file.rs
#
# Only cited files are checked, so this is not a tax on every refactor. Every
# test name in a cited file at the merge base must still exist in the head -
# in that file or any other tracked .rs file, so moving a test is not a
# deletion. A test that is genuinely meant to go is declared the same way:
#
#     Removes-Test: name_of_the_test
#
# A declared removal passes; an undeclared one fails and names the test.
#
# Recognised tests: a function following #[test], #[tokio::test],
# #[sqlx::test], #[rstest] or similar, with any other attributes (such as
# #[ignore]) in between.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

BASE_REF="${BASE_REF:-origin/testnet}"

if ! base="$(git merge-base "$BASE_REF" HEAD 2>/dev/null)"; then
  # "Could not look" is not "nothing found".
  echo "::error::cannot find a merge base between $BASE_REF and HEAD" >&2
  exit 1
fi

messages="$(git log --format=%B "$base..HEAD")" || exit 1
cited="$(printf '%s\n' "$messages" | sed -n 's/^Cites-Tests:[[:space:]]*//p' | sed 's/[[:space:]]*$//' | sort -u)"
removed="$(printf '%s\n' "$messages" | sed -n 's/^Removes-Test:[[:space:]]*//p' | sed 's/[[:space:]]*$//' | sort -u)"

if [ -z "$cited" ]; then
  echo "no Cites-Tests trailer between $BASE_REF and HEAD - nothing to compare"
  exit 0
fi

# Test function names, one per line, read from stdin.
test_names() {
  awk '
    /^[[:space:]]*#\[/ {
      if ($0 ~ /^[[:space:]]*#\[([A-Za-z_]+::)*(test|rstest)[]((]/) armed = 1
      next
    }
    /^[[:space:]]*(\/\/.*)?$/ { next }
    {
      if (armed && match($0, /fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
        s = substr($0, RSTART, RLENGTH); sub(/^fn[[:space:]]+/, "", s); print s
      }
      armed = 0
    }'
}

head_names="$(git ls-files -z -- '*.rs' | xargs -0 cat | test_names | sort -u)" || exit 1

fail=0
while IFS= read -r file; do
  [ -n "$file" ] || continue
  if ! git cat-file -e "$base:$file" 2>/dev/null; then
    echo "note: $file does not exist at the merge base - nothing to compare"
    continue
  fi
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    printf '%s\n' "$head_names" | grep -qxF -- "$name" && continue
    printf '%s\n' "$removed" | grep -qxF -- "$name" && {
      echo "ok: $name removed from $file, and declared"
      continue
    }
    echo "::error file=$file::test '$name' exists at the merge base and is gone from HEAD" >&2
    fail=1
  done < <(git show "$base:$file" | test_names | sort -u)
done <<< "$cited"

if [ "$fail" -ne 0 ]; then
  echo >&2
  echo "A cited test disappeared. If a merge did this, restore it. If the removal" >&2
  echo "is intended, add a 'Removes-Test: <name>' line to a commit message." >&2
  exit 1
fi
echo "every test in the cited files is still present"
