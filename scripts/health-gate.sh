#!/bin/bash
# Post-deploy health gate for ethpayserver.
#
# Polls /health/deep every 5 seconds for up to HEALTH_TIMEOUT seconds.
# Succeeds when:
#   1. The endpoint returns 200
#   2. build_sha matches EXPECTED_SHA (if set)
#   3. The x-sentry-release response header matches build_sha
#   4. Postgres and Redis report "ok"
#   5. All RPC chains report "ok" (no chain in error/disconnected state)
#   6. monitor.data_fresh is true
#   7. When monitor.data_fresh is true (evmmonitor is up and reporting), the
#      x-evmmonitor-sentry-release response header also matches build_sha
#
# (5) does not imply (6): an empty `rpcs` map — evmmonitor unreachable, or the
# chain-health fetch itself erroring — has no chain to name as bad, so it
# passes (5) vacuously while data_fresh is false. That gap let a cutover pass
# with the monitor not actually reporting anything.
#
# (3) exists because build_sha and the Sentry release are set by two separate
# CI steps from the same commit sha, so they can drift apart without either
# build step failing: a later stage that rebuilds from source without
# re-exporting SENTRY_RELEASE ships a binary with a correct build_sha and no
# release tag on the errors it sends. Comparing the two on the *running*
# process, not the build log, is the only way to catch that on the deployed
# artefact rather than the build that produced it.
#
# (7) is the same drift, for evmmonitor: a second binary, built in a separate
# CI step from the same commit sha, that tags its own Sentry events from its
# own compiled SENTRY_RELEASE. It has no HTTP endpoint of its own, so its
# release is relayed onto this response rather than checked directly. Gated
# on data_fresh rather than checked unconditionally: when evmmonitor isn't
# reporting at all, (6) already fails the gate, and a stale or absent header
# from an unreachable monitor shouldn't be reported as its own distinct
# failure.
#
# If the gate does not pass within the timeout, exit 1 — the previous
# container image stays live (Docker Compose health-check prevents cutover).
#
# A timeout caused by a build_sha mismatch is reported as one of three
# distinct situations, not a single generic failure: no response ever
# reported a build_sha at all (the endpoint may be unreachable, or the deploy
# just hasn't started answering yet), the sha reported has not changed at any
# point during the poll (the new build hasn't landed yet), or the sha changed
# but to something other than EXPECTED_SHA (the running build is neither the
# old one nor the one that was deployed - the only case of the three that
# actually means something is wrong). The first two call for waiting and
# re-checking; the third calls for investigating. A flat "SHA mismatch"
# timeout message cannot tell a reader which one they are looking at, and a
# deploy that was simply slower than the budget reads identically to a
# deploy that never happened.
#
# Required env vars:
#   HEALTH_URL       — full URL to the /health/deep endpoint
#
# Optional:
#   HEALTH_TIMEOUT   — seconds to poll (default: 60)
#   EXPECTED_SHA     — expected build_sha in the response

set -u
set -o pipefail

: "${HEALTH_URL:?HEALTH_URL required — e.g. https://api.agent8.cash/health/deep}"
: "${HEALTH_TIMEOUT:=60}"
: "${EXPECTED_SHA:=}"

INTERVAL=5
ELAPSED=0
HEADERS_FILE=$(mktemp)
trap 'rm -f "$HEADERS_FILE"' EXIT

# Set under `set -u` so a timeout where curl never once succeeds (nothing
# below ever assigns these) reports the "no response" case on exit instead of
# dying on an unbound variable.
BUILD_SHA=""
SENTRY_RELEASE_HDR=""
EVMMONITOR_SENTRY_RELEASE_HDR=""
PG_STATUS="error"
REDIS_STATUS="error"
MONITOR_FRESH="False"
RPC_BAD="unknown"

# Tracks whether the reported build_sha ever changed during the poll, so a
# timeout can distinguish "stuck on the old build" from "moved to a third,
# unexpected build" instead of reporting both as the same "SHA mismatch".
FIRST_SEEN_SHA=""
SHA_EVER_CHANGED=false

log() { printf '[health-gate] %s\n' "$*"; }

while [[ $ELAPSED -lt $HEALTH_TIMEOUT ]]; do
  BODY=$(curl -sS --max-time 10 -D "$HEADERS_FILE" "$HEALTH_URL" 2>&1) || {
    log "curl failed (elapsed ${ELAPSED}s), retrying..."
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  }

  # Parse response fields
  BUILD_SHA=$(echo "$BODY" | python3 -c "import json,sys; print(json.load(sys.stdin).get('build_sha',''))" 2>/dev/null || echo "")
  SENTRY_RELEASE_HDR=$(tr -d '\r' < "$HEADERS_FILE" | awk -F': ' 'tolower($1) == "x-sentry-release" { print $2 }')
  EVMMONITOR_SENTRY_RELEASE_HDR=$(tr -d '\r' < "$HEADERS_FILE" | awk -F': ' 'tolower($1) == "x-evmmonitor-sentry-release" { print $2 }')
  PG_STATUS=$(echo "$BODY" | python3 -c "import json,sys; print(json.load(sys.stdin)['postgres']['status'])" 2>/dev/null || echo "error")
  REDIS_STATUS=$(echo "$BODY" | python3 -c "import json,sys; print(json.load(sys.stdin)['redis']['status'])" 2>/dev/null || echo "error")
  MONITOR_FRESH=$(echo "$BODY" | python3 -c "import json,sys; print(json.load(sys.stdin)['monitor']['data_fresh'])" 2>/dev/null || echo "False")

  if [[ -n "$BUILD_SHA" ]]; then
    if [[ -z "$FIRST_SEEN_SHA" ]]; then
      FIRST_SEEN_SHA="$BUILD_SHA"
    elif [[ "$BUILD_SHA" != "$FIRST_SEEN_SHA" ]]; then
      SHA_EVER_CHANGED=true
    fi
  fi

  # Check RPC chains — all must be "ok"
  RPC_BAD=$(echo "$BODY" | python3 -c "
import json, sys
data = json.load(sys.stdin)
bad = [k for k, v in data.get('rpcs', {}).items() if v.get('status') != 'ok']
print(','.join(bad) if bad else '')
" 2>/dev/null || echo "unknown")

  # Check build SHA if expected
  if [[ -n "$EXPECTED_SHA" && "$BUILD_SHA" != "$EXPECTED_SHA" ]]; then
    log "SHA mismatch: got=$BUILD_SHA expected=$EXPECTED_SHA (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  # The Sentry release and build_sha are compiled in by two separate CI
  # steps from the same commit sha, so they can drift apart (rename, typo, a
  # rebuild stage that drops the env var) without either build step failing.
  # Comparing them on the running process is what actually proves the fix
  # reached the deployed binary rather than just the build that produced it.
  # The explicit empty check matters: without it, a malformed response with
  # no build_sha and a missing header would satisfy "" == "" and pass
  # vacuously - reporting a match when nothing was actually verified.
  if [[ -z "$SENTRY_RELEASE_HDR" || "$SENTRY_RELEASE_HDR" != "$BUILD_SHA" ]]; then
    log "x-sentry-release mismatch: got='$SENTRY_RELEASE_HDR' build_sha='$BUILD_SHA' (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  # Check all dependencies
  if [[ "$PG_STATUS" != "ok" ]]; then
    log "postgres=$PG_STATUS (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  if [[ "$REDIS_STATUS" != "ok" ]]; then
    log "redis=$REDIS_STATUS (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  if [[ -n "$RPC_BAD" ]]; then
    log "unhealthy chains: $RPC_BAD (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  if [[ "$MONITOR_FRESH" != "True" ]]; then
    log "monitor.data_fresh=$MONITOR_FRESH (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  # evmmonitor is a second binary, built from the same commit sha in its own
  # CI step, that tags its own Sentry events from its own compiled
  # SENTRY_RELEASE - the same drift the (3) check above catches for
  # ethpayserver is just as possible here, and evmmonitor has no HTTP
  # endpoint of its own to check directly. Gated on data_fresh (already
  # confirmed true above): an unreachable evmmonitor already fails the gate
  # on that check, so a missing header here would only restate it.
  if [[ -z "$EVMMONITOR_SENTRY_RELEASE_HDR" || "$EVMMONITOR_SENTRY_RELEASE_HDR" != "$BUILD_SHA" ]]; then
    log "x-evmmonitor-sentry-release mismatch: got='$EVMMONITOR_SENTRY_RELEASE_HDR' build_sha='$BUILD_SHA' (elapsed ${ELAPSED}s)"
    sleep $INTERVAL
    ELAPSED=$((ELAPSED + INTERVAL))
    continue
  fi

  # All checks passed
  log "HEALTHY — sha=$BUILD_SHA sentry_release=$SENTRY_RELEASE_HDR evmmonitor_sentry_release=$EVMMONITOR_SENTRY_RELEASE_HDR pg=ok redis=ok rpcs=all_ok monitor.data_fresh=true (${ELAPSED}s)"
  exit 0
done

if [[ -n "$EXPECTED_SHA" && "$BUILD_SHA" != "$EXPECTED_SHA" ]]; then
  if [[ -z "$FIRST_SEEN_SHA" ]]; then
    log "TIMEOUT after ${HEALTH_TIMEOUT}s — no response ever reported a build_sha; cannot tell whether the deploy hasn't landed yet or the endpoint is unreachable"
  elif [[ "$SHA_EVER_CHANGED" == false ]]; then
    log "TIMEOUT after ${HEALTH_TIMEOUT}s — sha stayed at $FIRST_SEEN_SHA for the whole poll; the new build hasn't landed yet"
  else
    log "TIMEOUT after ${HEALTH_TIMEOUT}s — sha changed during the poll (from $FIRST_SEEN_SHA, last seen $BUILD_SHA) but never reached the expected $EXPECTED_SHA; investigate"
  fi
else
  log "TIMEOUT after ${HEALTH_TIMEOUT}s — deploy health gate FAILED"
fi
log "Last response: pg=$PG_STATUS redis=$REDIS_STATUS rpc_bad=$RPC_BAD monitor_fresh=$MONITOR_FRESH sha=$BUILD_SHA sentry_release=$SENTRY_RELEASE_HDR evmmonitor_sentry_release=$EVMMONITOR_SENTRY_RELEASE_HDR"
exit 1
