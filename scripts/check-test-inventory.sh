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
# Which files are checked: every .rs file the change touches (modified or
# deleted since the merge base), plus any file named by a trailer line in a
# commit message,
#
#     Cites-Tests: path/to/file.rs
#
# The check used to run only for cited files. Nobody ever wrote the trailer, so
# it protected nothing; an opt-in guard that is never opted into is no guard. A
# file the change does not touch cannot lose a test, so touching is the scope,
# and a refactor that keeps its tests pays nothing. The trailer remains for a
# file the change should be held to even though its diff does not show it
# (a merge that resolved the file back to the base's copy, say).
#
# Every test name in a checked file at the merge base must still exist in the
# head - in that file or any other tracked .rs file, so moving a test is not a
# deletion. A test renamed in place is not a deletion either: its body is
# unchanged under a new name that did not exist before. A test that is
# genuinely meant to go is declared in a commit message:
#
#     Removes-Test: name_of_the_test
#
# A declared removal passes; an undeclared one fails and names the test, even
# if the file also gained tests.
#
# A cited file in which no test is recognised at the merge base fails, because
# it means the parser could not see them, not that there was nothing to lose.
# A merely touched file with no tests is skipped.
#
# Recognised tests: a function following #[test], #[tokio::test],
# #[sqlx::test], #[rstest] or similar, with any other attributes (such as
# #[ignore]) in between. Matching is by name across all tracked .rs files, so
# a deleted test whose name is also used elsewhere counts as present: that is
# the price of treating a move as not-a-deletion.
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

touched="$(git diff --name-only --no-renames --diff-filter=MD "$base" HEAD -- '*.rs')" || exit 1

# Test function names, one per line, read from stdin. An attribute may span
# several lines (#[sqlx::test(\n migrations = ...\n)]), so it is read until its
# brackets balance; a test whose attribute is not read to the end is invisible,
# which is exactly the silent pass this exists to prevent.
test_names() {
  awk '
    # Output is "name<TAB>body with whitespace removed", so a rename in place
    # can be told from a deletion.
    function feed(text,   i, ch) {
      for (i = 1; i <= length(text); i++) {
        ch = substr(text, i, 1)
        if (ch == "{") { bd++; seen = 1 } else if (ch == "}") bd--
        if (ch !~ /[[:space:]]/) body = body ch
        if (seen && bd <= 0) { print cur "\t" body; pending = 0; return }
      }
    }
    function emit(line,   s, rest) {
      if (match(line, /fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/)) {
        s = substr(line, RSTART, RLENGTH); sub(/^fn[[:space:]]+/, "", s)
        rest = substr(line, RSTART + RLENGTH)
        cur = s; body = ""; bd = 0; seen = 0; pending = 1
        feed(rest)
        return 1
      }
      return 0
    }
    function depth_delta(line,   t, o, c) {
      t = line; o = gsub(/\[/, "", t); c = gsub(/\]/, "", t); return o - c
    }
    pending { feed($0); next }
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
    }
    END { if (pending) print cur "\t" body }'
}

head_tests="$(git ls-files -z -- '*.rs' | xargs -0 cat | test_names | sort -u)" || exit 1
head_names="$(cut -f1 <<<"$head_tests")"

# Files to check: "cited" files are strict, "touched" ones are skipped when
# they hold no tests.
files="$( { printf '%s\n' "$cited"; printf '%s\n' "$touched"; } | sed '/^$/d' | sort -u)"
if [ -z "$files" ]; then
  echo "no .rs file changed since $BASE_REF - nothing to compare"
  exit 0
fi

# Prints the name of a head test with this body whose name did not exist in
# the file at the merge base: the new name of a test renamed in place.
renamed_to() { # body base_names
  B="$1" awk -F'\t' '$2 == ENVIRON["B"] { print $1 }' <<<"$head_tests" |
    while IFS= read -r cand; do
      grep -qxF -- "$cand" <<<"$2" || { echo "$cand"; break; }
    done
}

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
  if ! base_tests="$(git show "$base:$file" | test_names | sort -u)"; then
    echo "::error file=$file::could not read the tests in $file at the merge base" >&2
    fail=1; continue
  fi
  base_names="$(cut -f1 <<<"$base_tests")"
  if [ -z "$base_names" ]; then
    # A touched file that never had tests has nothing to lose.
    grep -qxF -- "$file" <<<"$cited" || continue
    # A file the parser sees no tests in is a parser miss (an attribute form it
    # does not know, say), not a file with nothing to lose.
    echo "::error file=$file::no tests recognised in $file at the merge base, so nothing would be compared" >&2
    fail=1; continue
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
    body="$(awk -F'\t' -v n="$name" '$1 == n { print $2; exit }' <<<"$base_tests")"
    if [ -n "$body" ] && to="$(renamed_to "$body" "$base_names")" && [ -n "$to" ]; then
      echo "ok: $name renamed to $to in $file"
      continue
    fi
    missing+=("$name")
  done <<<"$base_names"
  # Every missing name is a deletion. Gaining tests does not excuse losing
  # one: a change justified by tests normally adds some, and a bad merge that
  # drops a landed test must not be covered by them.
  if [ "${#missing[@]}" -gt 0 ]; then
    for name in "${missing[@]}"; do
      echo "::error file=$file::test '$name' exists at the merge base and is gone from HEAD" >&2
    done
    fail=1
  fi
done <<< "$files"

if [ "$fail" -ne 0 ]; then
  echo >&2
  echo "A cited test disappeared. If a merge did this, restore it. If the removal" >&2
  echo "is intended, add a 'Removes-Test: <name>' line to a commit message." >&2
  exit 1
fi
echo "every test in the cited files is still present"
