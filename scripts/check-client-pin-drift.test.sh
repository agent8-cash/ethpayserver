#!/usr/bin/env bash
# Proves check-client-pin-drift.sh actually refuses to run blind.
#
# The incident this guards against: CLIENT_REPO pointed at an owner GitHub no
# longer resolves (a 404, or a redirect curl -f won't follow without -L), the
# branches/main lookup came back empty, and the script warned and exited 0 -
# so the drift check looked like it had run on every single CI build and
# never had. Nothing here exercised that path until now, same as the sentry
# and health-gate guards above it.
#
# Runs a real local HTTP server standing in for api.github.com, so the guard
# is exercised against real HTTP responses (including a real 404) rather
# than a hand-rolled stand-in for curl's behaviour.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GUARD="$HERE/check-client-pin-drift.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"; kill "${SRV:-}" 2>/dev/null' EXIT

PIN_FILE="$TMP/client-image.pin"
PORT=$((9000 + RANDOM % 900))

# Stands in for api.github.com: the one repo named "good/payserver-client"
# resolves, exactly as a real repo would; anything else 404s, exactly as a
# renamed or mistyped owner does.
cat > "$TMP/server.py" <<'PY'
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(s):
        if s.path == "/repos/good/payserver-client/branches/main":
            body = b'{"commit":{"sha":"abcdef0123456789abcdef0123456789abcdef01"}}'
        elif s.path == "/repos/good/payserver-client/compare/deadbee...abcdef0123456789abcdef0123456789abcdef01":
            body = b'{"ahead_by": 3}'
        else:
            s.send_response(404)
            s.send_header("Content-Length", "0")
            s.end_headers()
            return
        s.send_response(200)
        s.send_header("Content-Type", "application/json")
        s.send_header("Content-Length", str(len(body)))
        s.end_headers()
        s.wfile.write(body)
    def log_message(s, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

python3 "$TMP/server.py" "$PORT" &
SRV=$!
for _ in $(seq 1 50); do
  curl -fsS -o /dev/null "http://127.0.0.1:$PORT/repos/good/payserver-client/branches/main" 2>/dev/null && break
  sleep 0.1
done

pass=0
fail=0

run_guard() {  # client_repo
  env GITHUB_API="http://127.0.0.1:$PORT" CLIENT_REPO="$1" PIN_FILE="$PIN_FILE" GITHUB_TOKEN="" \
    "$GUARD" >"$TMP/out" 2>&1
}

assert_exit() {  # desc want client_repo
  local desc="$1" want="$2" repo="$3" rc=0
  run_guard "$repo" || rc=$?
  if [ "$rc" -eq "$want" ]; then
    echo "ok: $desc (exit $rc)"
    pass=$((pass + 1))
  else
    echo "FAIL: $desc - wanted exit $want, got $rc"
    sed 's/^/  /' "$TMP/out"
    fail=$((fail + 1))
  fi
}

echo "ghcr.io/x/payserver-client:sha-deadbee" > "$PIN_FILE"

assert_exit "a repo that resolves measures drift and exits 0" 0 "good/payserver-client"
assert_exit "a repo that cannot resolve (404, e.g. a stale owner) fails loudly" 1 "wrong/payserver-client"

if ! run_guard "wrong/payserver-client" && grep -q '::error::' "$TMP/out"; then
  echo "ok: the 404 case prints ::error:: rather than ::warning::"
  pass=$((pass + 1))
else
  echo "FAIL: the 404 case did not print ::error::"
  sed 's/^/  /' "$TMP/out"
  fail=$((fail + 1))
fi

# The pin-file guards are a different failure mode (the pin itself is bad,
# not the client repo) and stay non-fatal - unchanged by this ticket.
echo "" > "$PIN_FILE"
assert_exit "an empty pin file still warns and exits 0" 0 "good/payserver-client"

echo "ghcr.io/x/payserver-client:latest" > "$PIN_FILE"
assert_exit "a pin with no sha-<commit> tag still warns and exits 0" 0 "good/payserver-client"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
