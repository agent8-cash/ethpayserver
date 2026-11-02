#!/usr/bin/env bash
# Refuse a commons pin that is not on payserver-commons main.
#
# A cross-repo ticket pins commons by branch sha before the commons PR merges.
# Merge ethpayserver first (or forget to re-pin) and the workspace ends up
# pinned to a commit that lives only on the commons branch - and once that
# branch is deleted by a squash merge, the commit is gone for good. Nothing
# fails at merge time: the build stays green until the next clean checkout
# tries to resolve the pin and finds nothing there.
#
# Commons is public, so this needs no token - an anonymous, blobless clone of
# `main` is enough to walk its commit graph.
#
# Must-fail-first is not a claim to take on faith here: run
# check-commons-pin.test.sh, which builds a real throwaway commons history
# (a main commit and an unmerged branch commit) and asserts this script goes
# red against the branch-only sha, green against the main sha, red on a
# partial re-pin, and red on a short sha - the same shape as the incident
# this guard exists to catch, reproduced on every run instead of asserted
# once in a comment.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$repo_root/Cargo.toml"
# Overridable so the retry-and-give-up path can actually be exercised. A branch
# that only runs during an outage is the kind that turns out not to work during
# one.
url="${COMMONS_URL:-https://github.com/agent8-cash/payserver-commons.git}"

# Every `rev = "..."` on a payserver-commons line, deduped. One revision is
# shared by all crates in practice, but read them all rather than assume it -
# a PR that moves only some lines is exactly the mistake worth catching.
# `|| true` matters more than it looks. Under `set -euo pipefail` a grep that
# matches nothing fails the whole substitution and exits BEFORE the guard below
# can run - so the explanatory error could never print, and a manifest whose URL
# lacked the `.git` suffix (cargo accepts both forms) produced exit 1 with no
# output at all. For a check whose entire job is explaining a cross-repo
# mistake, "Process completed with exit code 1" is the worst possible message.
commons_lines="$(grep 'payserver-commons' "$manifest" || true)"
# Capture the whole `rev = "..."` value, not just an already-40-hex match -
# a short sha (cargo accepts one) must be caught and named, not skipped as if
# the line had no rev at all.
rev_values="$(printf '%s\n' "$commons_lines" | grep -oP 'rev = "\K[^"]*' | sort -u || true)"

revs=""
malformed=""
while IFS= read -r v; do
  [ -z "$v" ] && continue
  if [[ "$v" =~ ^[0-9a-f]{40}$ ]]; then
    revs="$revs$v"$'\n'
  else
    malformed="$malformed$v"$'\n'
  fi
done <<< "$rev_values"
revs="${revs%$'\n'}"
malformed="${malformed%$'\n'}"

if [ -n "$malformed" ]; then
  echo "::error::payserver-commons rev is not a full 40-character sha:"
  printf '%s\n' "$malformed" | sed 's/^/  /'
  echo "  A short or malformed sha cannot be checked against commons main. Re-pin with the full sha: scripts/commons.sh pin <sha>"
  exit 1
fi

# A crate pinned by `branch =` or `tag =` is NOT pinned to a revision, and this
# check cannot say anything about where it points. Silently skipping it exits 0
# on precisely the state this guard exists to prevent: pinned to something that
# is not on main. Name it and fail.
loose="$(printf '%s\n' "$commons_lines" | grep -E '(branch|tag) *= *"' || true)"
if [ -n "$loose" ]; then
  echo "::error::payserver-commons must be pinned by rev, not by branch or tag."
  printf '%s\n' "$loose" | sed 's/^/  /'
  echo "  A branch or tag can move; a rev cannot. Use: scripts/commons.sh pin <sha>"
  exit 1
fi

[ -n "$revs" ] || { echo "::error::no payserver-commons rev found in $manifest"; exit 1; }

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

echo "fetching payserver-commons main..."
# --single-branch: only main's history, not every branch in the repo.
# --filter=blob:none: commits and trees, not file contents - all a commit-graph
# walk needs. No --depth: a shallow clone truncates the graph, so a pin older
# than the truncation point has no local object to check against and reads as
# "not on main" - failing the build over a perfectly good, merged pin.
# Retried, because this step gates every other job in CI - test, audit, build,
# e2e, docker and both deploy notifies all have a `needs:` path back to lint. A
# transient github.com blip red-lighting all of them is a worse failure than the
# one this check exists to catch. The apt step in the same workflow already
# carries a retry loop for exactly this reason.
cloned=0
clone_err=""
for attempt in 1 2 3; do
  # Same `if` shape as the merge-base call below, for the same reason: a bare
  # assignment would let `set -e` exit before $clone_err could be read.
  if clone_err="$(timeout "${COMMONS_CLONE_TIMEOUT:-45}" git clone -q --single-branch --branch main --filter=blob:none \
      --no-checkout "$url" "$workdir/commons" 2>&1)"; then
    cloned=1
    break
  fi
  rm -rf "$workdir/commons"
  echo "::warning::clone of payserver-commons failed (attempt $attempt of 3): $clone_err"
  [ "$attempt" -lt 3 ] && sleep $((attempt * 5))
done
if [ "$cloned" != 1 ]; then
  echo "::error::could not clone payserver-commons after 3 attempts - cannot verify the pin"
  echo "  last error: $clone_err"
  exit 1
fi

fail=0
while IFS= read -r rev; do
  # --is-ancestor returns 1 for "not an ancestor" and >1 for a git error.
  # Collapsing both into one branch tells a developer to re-pin a perfectly good
  # merged sha when the real problem is a broken clone.
  # `err=$(...)` on its own line would exit the script under `set -e` before
  # $? could be read - the very bug this block exists to fix, reintroduced one
  # line lower. An `if` context suppresses errexit; that is why this is shaped
  # like this and not like an assignment.
  if err="$(git -C "$workdir/commons" merge-base --is-ancestor "$rev" origin/main 2>&1)"; then
    rc=0
  else
    rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    echo "ok: $rev is on payserver-commons main"
  elif [ "$rc" -eq 1 ]; then
    fail=1
    echo "::error::payserver-commons rev $rev is not on commons main."
    echo "  Merge the commons PR first, then re-pin with: scripts/commons.sh pin <sha>"
  elif [ "$rc" -eq 128 ]; then
    # git cannot resolve the object at all - a typo, or a commit that only ever
    # existed on a branch this clone does not have. Either way it is not on
    # main, which is what the caller needs to know.
    fail=1
    echo "::error::payserver-commons rev $rev does not exist on commons main."
    echo "  Either the sha is wrong, or it only exists on an unmerged branch."
    echo "  Merge the commons PR first, then re-pin with: scripts/commons.sh pin <sha>"
  else
    fail=1
    echo "::error::could not check $rev against commons main (git exit $rc): $err"
    echo "  This is a problem with the check, not necessarily with the pin."
  fi
done <<< "$revs"

exit "$fail"
