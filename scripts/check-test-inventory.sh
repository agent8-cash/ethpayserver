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
# A declared removal passes; an undeclared one fails and names the test. A
# file that loses no more tests than it gained (a rename in place) passes: the
# check is on NET deletion.
#
# A cited file in which no test is recognised at the merge base fails, because
# it means the parser could not see them, not that there was nothing to lose.
#
# Recognised tests: a function following #[test], #[tokio::test],
# #[sqlx::test], #[rstest] or similar, with any other attributes (such as
# #[ignore]) in between. Matching is by name across all tracked .rs files, so
# a deleted test whose name is also used elsewhere counts as present: that is
# the price of treating a move as not-a-deletion. A test renamed in place has
# lost its old name and must be declared with Removes-Test.
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

# Test function names, one per line, read from stdin. An attribute may span
# several lines (#[sqlx::test(\n migrations = ...\n)]), so it is read until its
# brackets balance; a test whose attribute is not read to the end is invisible,
# which is exactly the silent pass this exists to prevent.
test_names() {
  awk '
    function emit(line) {
      if (match(line, /fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
        s = substr(line, RSTART, RLENGTH); sub(/^fn[[:space:]]+/, "", s); print s
        return 1
      }
      return 0
    }
    function depth_delta(line,   t, o, c) {
      t = line; o = gsub(/\[/, "", t); c = gsub(/\]/, "", t); return o - c
    }
    depth > 0 {
      attr = attr " " $0; depth += depth_delta($0)
      if (depth <= 0) {
        depth = 0
        if (attr ~ /^[[:space:]]*#\[([A-Za-z_]+::)*(test|rstest)[]((]/) armed = 1
        rest = $0; sub(/^[^]]*\]/, "", rest)
        if (armed && emit(rest)) armed = 0
      }
      next
    }
    /^[[:space:]]*#\[/ {
      attr = $0; depth = depth_delta($0)
      if (depth <= 0) {
        depth = 0
        if (attr ~ /^[[:space:]]*#\[([A-Za-z_]+::)*(test|rstest)[]((]/) armed = 1
        rest = $0; sub(/^[[:space:]]*#\[[^]]*\]/, "", rest)
        if (armed && emit(rest)) armed = 0
      }
      next
    }
    /^[[:space:]]*(\/\/.*)?$/ { next }
    {
      if (armed) emit($0)
      armed = 0
    }'
}

head_names="$(git ls-files -z -- '*.rs' | xargs -0 cat | test_names | sort -u)" || exit 1

fail=0
while IFS= read -r file; do
  [ -n "$file" ] || continue
  case "$file" in
    *.rs) ;;
    *) echo "::error::Cites-Tests path '$file' is not a .rs file, so nothing would be compared" >&2
       fail=1; continue ;;
  esac
  if ! git cat-file -e "$base:$file" 2>/dev/null; then
    # Absent from the base AND the head is a typo or a rename, not a new file:
    # "could not look" must not read as "nothing found".
    if ! git cat-file -e "HEAD:$file" 2>/dev/null; then
      echo "::error::Cites-Tests path '$file' exists neither at the merge base nor in HEAD" >&2
      fail=1
    else
      echo "note: $file is new since the merge base - nothing to compare"
    fi
    continue
  fi
  # Captured, not piped into the loop: a failure inside a process substitution
  # is lost, and "could not enumerate" must not read as "nothing deleted".
  if ! base_names="$(git show "$base:$file" | test_names | sort -u)"; then
    echo "::error file=$file::could not read the tests in $file at the merge base" >&2
    fail=1; continue
  fi
  if [ -z "$base_names" ]; then
    # A file the parser sees no tests in is a parser miss (an attribute form it
    # does not know, say), not a file with nothing to lose.
    echo "::error file=$file::no tests recognised in $file at the merge base, so nothing would be compared" >&2
    fail=1; continue
  fi
  file_head_names=""
  if git cat-file -e "HEAD:$file" 2>/dev/null; then
    file_head_names="$(git show "HEAD:$file" | test_names | sort -u)" || { fail=1; continue; }
  fi
  # Names gone from the head everywhere and not declared.
  missing=()
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    grep -qxF -- "$name" <<<"$head_names" && continue
    grep -qxF -- "$name" <<<"$removed" && {
      echo "ok: $name removed from $file, and declared"
      continue
    }
    missing+=("$name")
  done <<<"$base_names"
  # A deletion is a NET deletion: names this file gained are renames (or
  # replacements) and cover that many missing names. Names alone cannot tell a
  # rename from a delete plus an add, and firing on renames would get the
  # check disabled.
  added="$(comm -13 <(printf '%s\n' "$base_names") <(printf '%s\n' "$file_head_names") | grep -c . )"
  if [ "${#missing[@]}" -gt "$added" ]; then
    for name in "${missing[@]}"; do
      echo "::error file=$file::test '$name' exists at the merge base and is gone from HEAD" >&2
    done
    fail=1
  elif [ "${#missing[@]}" -gt 0 ]; then
    echo "note: ${missing[*]} gone from $file, covered by $added test(s) it gained (rename)"
  fi
done <<< "$cited"

if [ "$fail" -ne 0 ]; then
  echo >&2
  echo "A cited test disappeared. If a merge did this, restore it. If the removal" >&2
  echo "is intended, add a 'Removes-Test: <name>' line to a commit message." >&2
  exit 1
fi
echo "every test in the cited files is still present"
