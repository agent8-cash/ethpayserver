#!/usr/bin/env bash
# Proves dispatch-deploy-testnet.sh actually fails the step on a redirect or
# an error response, instead of the `curl --fail-with-body` (no `-L`) it used
# to run, which exits 0 on a 3xx because it never follows the redirect and a
# redirect isn't an HTTP error by curl's `-f` definition. That is exactly how
# a merge used to reach this job, dispatch to a repo that 307-redirects every
# request, exit 0, and never deploy anything - with nothing in the run
# distinguishing it from a real dispatch.
#
# Runs a real local HTTP server standing in for api.github.com, so the guard
# is exercised against real HTTP responses (including a real 307) rather than
# a hand-rolled stand-in for curl's behaviour.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GUARD="$HERE/dispatch-deploy-testnet.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"; kill "${SRV:-}" 2>/dev/null' EXIT

PORT=$((9000 + RANDOM % 900))
FULL_SHA="abcdef0123456789abcdef0123456789abcdef01"
SHA_TAG="sha-abcdef0"

# Stands in for api.github.com: /repos/good/repo/dispatches accepts exactly
# as the real API would (202); /repos/old/repo/dispatches 307-redirects to
# the renamed owner, exactly as the old owner does today; /repos/missing/repo
# 404s, exactly as a mistyped or unreachable repo does.
cat > "$TMP/server.py" <<'PY'
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(s):
        if s.path == "/repos/good/repo/dispatches":
            s.send_response(202)
            s.send_header("Content-Length", "0")
            s.end_headers()
        elif s.path == "/repos/old/repo/dispatches":
            s.send_response(307)
            s.send_header("Location", "https://api.github.com/repos/good/repo/dispatches")
            s.send_header("Content-Length", "0")
            s.end_headers()
        else:
            body = b'{"message":"Not Found"}'
            s.send_response(404)
            s.send_header("Content-Length", str(len(body)))
            s.end_headers()
            s.wfile.write(body)
    def log_message(s, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

python3 "$TMP/server.py" "$PORT" &
SRV=$!
for _ in $(seq 1 50); do
  curl -fsS -o /dev/null -X POST "http://127.0.0.1:$PORT/repos/good/repo/dispatches" 2>/dev/null && break
  sleep 0.1
done

pass=0
fail=0

run_guard() {  # dispatch_repo sha_tag full_sha pat
  env GITHUB_API="http://127.0.0.1:$PORT" DISPATCH_REPO="$1" SHA_TAG="$2" GITHUB_SHA="$3" RCS_DISPATCH_PAT="$4" \
    "$GUARD" >"$TMP/out" 2>&1
}

assert_exit() {  # desc want dispatch_repo sha_tag full_sha pat
  local desc="$1" want="$2" rc=0
  run_guard "$3" "$4" "$5" "$6" || rc=$?
  if [ "$rc" -eq "$want" ]; then
    echo "ok: $desc (exit $rc)"
    pass=$((pass + 1))
  else
    echo "FAIL: $desc - wanted exit $want, got $rc"
    sed 's/^/  /' "$TMP/out"
    fail=$((fail + 1))
  fi
}

assert_exit "a 2xx response dispatches and exits 0" 0 \
  "good/repo" "$SHA_TAG" "$FULL_SHA" "token"

assert_exit "a 307 (old-owner redirect, not followed) fails loudly" 1 \
  "old/repo" "$SHA_TAG" "$FULL_SHA" "token"

assert_exit "a 404 (unresolvable repo) fails loudly" 1 \
  "missing/repo" "$SHA_TAG" "$FULL_SHA" "token"

if ! run_guard "old/repo" "$SHA_TAG" "$FULL_SHA" "token" && grep -q '::error::' "$TMP/out"; then
  echo "ok: the redirect case prints ::error:: with the http code, not a silent pass"
  pass=$((pass + 1))
else
  echo "FAIL: the redirect case did not print ::error::"
  sed 's/^/  /' "$TMP/out"
  fail=$((fail + 1))
fi

assert_exit "no RCS_DISPATCH_PAT fails before any request" 1 \
  "good/repo" "$SHA_TAG" "$FULL_SHA" ""

assert_exit "no SHA_TAG fails before any request" 1 \
  "good/repo" "" "$FULL_SHA" "token"

assert_exit "a GITHUB_SHA that isn't a 40-char sha fails before any request" 1 \
  "good/repo" "$SHA_TAG" "abc123" "token"

assert_exit "a sha tag that doesn't belong to the commit fails before any request" 1 \
  "good/repo" "sha-0000000" "$FULL_SHA" "token"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
