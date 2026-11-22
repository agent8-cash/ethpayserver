#!/usr/bin/env bash
# Refuses a tracked source file that carries a ticket id or a tracker link.
#
# This repository is public. A ticket id in the source leaks the title and
# shape of unreleased work to anyone reading it, and it is useless to the
# only audience the code has: an outside reader cannot open the tracker to
# make sense of it. The ticket id belongs in the commit message and the PR
# title. Write the reason the code exists, not a pointer to where someone
# once explained it.
#
# git grep, not bare grep: grep on this box is ugrep and honours
# .gitignore, which has already produced a false-clean leak scan.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1

# git grep exits 1 for "ran fine, no matches" and >1 for a real failure (bad
# pathspec, git itself erroring, ...). Those must not be conflated: swallowing
# every nonzero exit into "clean" would reproduce, one layer up, the exact
# false-clean-scan failure this check exists to catch.
matches="$(git grep -nIE 'RCS-[0-9]+|AG8-[0-9]+|linear\.app' -- . \
  ':!CLAUDE.md' ':!AGENTS.md' ':!docs' ':!scripts/check-no-ticket-refs.test.sh')"
status=$?

if [ "$status" -gt 1 ]; then
  echo "::error::git grep failed (exit $status) while scanning for ticket ids/tracker links" >&2
  exit 1
fi

if [ "$status" -eq 0 ]; then
  echo "::error::ticket id or tracker link found in tracked source files:" >&2
  echo "$matches" >&2
  echo >&2
  echo "Write the reason instead of the reference. The ticket id belongs in" >&2
  echo "the commit message and the PR title, not in the source tree." >&2
  exit 1
fi

echo "no ticket ids or tracker links in tracked source files"
