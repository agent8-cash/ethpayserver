#!/usr/bin/env bash
# Measures how far ops/client-image.pin is behind payserver-client main, and
# warns past MAX_COMMITS_BEHIND. Non-fatal by design for that one case - see
# the comment above the CI job that calls this.
#
# What is NOT optional: resolving CLIENT_REPO itself. A wrong owner, a 404, a
# redirect curl -f won't follow - any of those used to produce an empty
# CURRENT_SHA that this script warned about and then exited 0, so the drift
# check looked like it ran and never did. That is the exact failure mode
# check-client-pin-drift.test.sh proves this script no longer has: pointed at
# a repo that cannot resolve, it now exits non-zero.
#
# GITHUB_API, CLIENT_REPO and PIN_FILE are overridable so the test can point
# this at a local mock server instead of the real GitHub API.
set -uo pipefail

CLIENT_REPO="${CLIENT_REPO:-agent8-cash/payserver-client}"
# Arbitrary but documented: past this many commits behind, e2e is exercising
# a client build old enough that a shipped feature has no coverage. Raise or
# lower it here if it fires too often or too rarely.
MAX_COMMITS_BEHIND="${MAX_COMMITS_BEHIND:-20}"
GITHUB_API="${GITHUB_API:-https://api.github.com}"
PIN_FILE="${PIN_FILE:-ops/client-image.pin}"

CLIENT_IMAGE="$(grep -v '^[[:space:]]*#' "$PIN_FILE" | tr -d '[:space:]')"
if [ -z "$CLIENT_IMAGE" ]; then
  echo "::warning::$PIN_FILE is empty, skipping drift check"
  exit 0
fi

PIN_SHA="${CLIENT_IMAGE##*:sha-}"
if [ "$PIN_SHA" = "$CLIENT_IMAGE" ] || [ -z "$PIN_SHA" ]; then
  echo "::warning::$PIN_FILE ($CLIENT_IMAGE) is not a sha-<commit> tag, cannot measure drift against $CLIENT_REPO main"
  exit 0
fi

# --max-time bounds a stalled/slow API call: without it, curl can hang past
# this job's timeout-minutes and get killed as a failed step - exactly the
# hard failure this check exists to avoid for a transient API hiccup.
api() { curl -sf --max-time 10 -H "Authorization: Bearer ${GITHUB_TOKEN:-}" -H "Accept: application/vnd.github+json" "$GITHUB_API/repos/${CLIENT_REPO}/$1"; }

CURRENT_SHA="$(api branches/main | jq -r '.commit.sha // empty')"
if [ -z "$CURRENT_SHA" ]; then
  echo "::error::could not resolve $CLIENT_REPO main - pin-drift cannot be measured blind. Check CLIENT_REPO (is it the client's current owner/repo?) and GITHUB_TOKEN."
  exit 1
fi
CURRENT_TAG="sha-${CURRENT_SHA:0:7}"

AHEAD_BY="$(api "compare/${PIN_SHA}...${CURRENT_SHA}" | jq -r '.ahead_by // empty')"
if [ -z "$AHEAD_BY" ]; then
  echo "::warning::could not compare $PIN_FILE (sha-$PIN_SHA) against $CLIENT_REPO main, skipping drift check"
  exit 0
fi

echo "pinned:   sha-$PIN_SHA"
echo "current:  $CURRENT_TAG"
echo "distance: $AHEAD_BY commit(s) behind main"

if [ "$AHEAD_BY" -gt "$MAX_COMMITS_BEHIND" ]; then
  echo "::warning::$PIN_FILE is $AHEAD_BY commits behind payserver-client main (pinned sha-$PIN_SHA, current $CURRENT_TAG) - past $MAX_COMMITS_BEHIND, e2e may no longer exercise what the client actually ships. Bump it: sed -i '\$ s|.*|ghcr.io/gcannelloni/payserver-client:$CURRENT_TAG|' $PIN_FILE"
fi

exit 0
