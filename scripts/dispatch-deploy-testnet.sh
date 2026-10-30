#!/usr/bin/env bash
# Dispatches the testnet deploy event to DISPATCH_REPO, and fails the step
# loudly on anything other than a 2xx response.
#
# `curl --fail-with-body` (no `-L`) used to drive this: it treats a 3xx as
# success, because it never follows the redirect and a redirect on its own
# isn't an HTTP error by curl's `-f` definition. The repo this used to point
# at 307-redirects every request now, so that combination returned exit 0
# having dispatched nothing - a merge to testnet that silently never reached
# the running instance. Checking the status code ourselves is what makes a
# redirect (or a 404, or anything else that isn't 2xx) fail the step instead
# of looking like a successful dispatch. See dispatch-deploy-testnet.test.sh.
#
# DISPATCH_REPO and GITHUB_API are overridable so the test can point this at
# a local mock server instead of the real GitHub API.
set -uo pipefail

DISPATCH_REPO="${DISPATCH_REPO:-agent8-cash/central-infrastructure}"
GITHUB_API="${GITHUB_API:-https://api.github.com}"

[ -n "${RCS_DISPATCH_PAT:-}" ] || { echo "::error::RCS_DISPATCH_PAT is not set; images were pushed but nothing will deploy"; exit 1; }
[ -n "${SHA_TAG:-}" ] || { echo "::error::docker job produced no sha tag"; exit 1; }

# The signing certificate records the full commit it was issued for. Sending
# the full sha lets the receiver require that the signature was made for THIS
# commit, not merely by this workflow on this branch. Refuse, rather than send
# a payload without it, so the receiver never has to guess what absence means.
FULL_SHA="${GITHUB_SHA:-}"
[[ "$FULL_SHA" =~ ^[0-9a-f]{40}$ ]] || { echo "::error::GITHUB_SHA is not a 40-character sha: '$FULL_SHA'"; exit 1; }
[ "$SHA_TAG" = "sha-${FULL_SHA::7}" ] || { echo "::error::sha tag $SHA_TAG does not belong to commit $FULL_SHA"; exit 1; }

RESPONSE="$(curl -sS --max-time 20 -w '\n%{http_code}' -X POST \
  -H "Accept: application/vnd.github+json" \
  -H "Authorization: Bearer $RCS_DISPATCH_PAT" \
  "$GITHUB_API/repos/$DISPATCH_REPO/dispatches" \
  -d "{\"event_type\":\"deploy-testnet\",\"client_payload\":{\"sha_tag\":\"$SHA_TAG\",\"sha\":\"$FULL_SHA\"}}")"
HTTP_CODE="${RESPONSE##*$'\n'}"
BODY="${RESPONSE%$'\n'*}"

case "$HTTP_CODE" in
  2??) ;;
  *)
    echo "::error::dispatch to $DISPATCH_REPO got HTTP $HTTP_CODE, not 2xx (a redirect and an error both land here): $BODY"
    exit 1
    ;;
esac

echo "dispatched deploy-testnet $SHA_TAG ($FULL_SHA) to $DISPATCH_REPO"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "dispatched deploy-testnet $SHA_TAG ($FULL_SHA)" >> "$GITHUB_STEP_SUMMARY"
fi
