#!/usr/bin/env bash
# Proves cosign-sign.sh actually goes red and green on the cases it claims
# to, against a fake `cosign` on PATH rather than a comment asserting someone
# ran it once. In particular: a signing failure must name signing in the
# error, not the digest guard that already passed - that distinction is the
# whole reason this script exists, and nothing in normal CI operation
# exercises cosign failing, so nothing organically proves it.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$HERE/cosign-sign.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAKE_DIGEST="ghcr.io/agent8-cash/ethpayserver/evmmonitor@sha256:deadbeef"

fail=0

# A real cosign that signs successfully.
cat > "$TMP/cosign" <<'EOF'
#!/usr/bin/env bash
echo "$@" >> "$COSIGN_CALL_LOG"
exit 0
EOF
chmod +x "$TMP/cosign"

export COSIGN_CALL_LOG="$TMP/calls.log"
: > "$COSIGN_CALL_LOG"
if out="$(PATH="$TMP:$PATH" "$CHECK" monitor "$FAKE_DIGEST" false 2>&1)"; then
  if grep -q -- "--tlog-upload=false" "$COSIGN_CALL_LOG" && grep -q -- "$FAKE_DIGEST" "$COSIGN_CALL_LOG"; then
    echo "ok: a successful sign passes through tlog_upload=false and the digest"
  else
    echo "FAIL: a successful sign did not pass tlog_upload=false and the digest - got: $(cat "$COSIGN_CALL_LOG")"; fail=1
  fi
else
  echo "FAIL: a successful cosign sign is reported as a failure - $out"; fail=1
fi

: > "$COSIGN_CALL_LOG"
if out="$(PATH="$TMP:$PATH" "$CHECK" server "$FAKE_DIGEST" true 2>&1)"; then
  if grep -q -- "--tlog-upload=true" "$COSIGN_CALL_LOG"; then
    echo "ok: a release build passes through tlog_upload=true"
  else
    echo "FAIL: tlog_upload=true was not passed through - got: $(cat "$COSIGN_CALL_LOG")"; fail=1
  fi
else
  echo "FAIL: a successful cosign sign is reported as a failure - $out"; fail=1
fi

# A cosign that fails the way an unreachable transparency log does.
cat > "$TMP/cosign" <<'EOF'
#!/usr/bin/env bash
echo "Error: signing [...]: signing digest: signing bundle: error signing bundle" >&2
exit 1
EOF
chmod +x "$TMP/cosign"

out="$(PATH="$TMP:$PATH" "$CHECK" monitor "$FAKE_DIGEST" false 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  echo "FAIL: a failing cosign sign is reported as a success"; fail=1
elif ! printf '%s' "$out" | grep -q "::error::cosign sign failed for the monitor image"; then
  echo "FAIL: a signing failure's error does not name signing - got: $out"; fail=1
elif printf '%s' "$out" | grep -q "no digest"; then
  echo "FAIL: a signing failure's error blames the digest instead of signing - got: $out"; fail=1
else
  echo "ok: a signing failure's error names signing, not the digest"
fi

out="$(PATH="$TMP:$PATH" "$CHECK" server "" false 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then
  echo "FAIL: an empty digest is accepted"; fail=1
elif ! printf '%s' "$out" | grep -q "::error::no digest for the server image"; then
  echo "FAIL: an empty digest's error does not name the image - got: $out"; fail=1
else
  echo "ok: an empty digest is refused and names the image"
fi

out="$(PATH="$TMP:$PATH" "$CHECK" server "$FAKE_DIGEST" maybe 2>&1)"
rc=$?
if [ "$rc" -eq 0 ]; then echo "FAIL: an invalid tlog_upload value is accepted"; fail=1
else echo "ok: an invalid tlog_upload value is refused"; fi

if [ "$fail" -ne 0 ]; then
  echo "cosign-sign.sh does NOT behave as documented"; exit 1
fi
echo "cosign-sign.sh behaves as documented"
