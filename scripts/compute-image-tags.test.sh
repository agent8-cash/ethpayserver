#!/usr/bin/env bash
# Proves compute-image-tags.sh actually ties tlog_upload to the ref the way
# it claims to - a release tag (mainnet) keeps the transparency-log upload,
# everything else (testnet, main) drops it - rather than a comment asserting
# someone checked it once. The two branches here were, until this script, the
# same `if` this process used to hand-verify by eye.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$HERE/compute-image-tags.sh"
FULL_SHA="33b647bdeadbeefdeadbeefdeadbeefdeadbeef"

fail=0
check_output() {  # name ref ref_name want_tag want_tlog
  local name="$1" ref="$2" ref_name="$3" want_tag="$4" want_tlog="$5"
  local out got_tag got_tlog
  out="$("$CHECK" "$ref" "$ref_name" "$FULL_SHA")" || { echo "FAIL: $name - exited nonzero"; fail=1; return; }
  got_tag="$(printf '%s\n' "$out" | sed -n 's/^tag=//p')"
  got_tlog="$(printf '%s\n' "$out" | sed -n 's/^tlog_upload=//p')"
  if [ "$got_tag" != "$want_tag" ] || [ "$got_tlog" != "$want_tlog" ]; then
    echo "FAIL: $name - got tag=$got_tag tlog_upload=$got_tlog, want tag=$want_tag tlog_upload=$want_tlog"
    fail=1
  else
    echo "ok: $name (tag=$got_tag tlog_upload=$got_tlog)"
  fi
}

check_output "a release tag keeps the transparency log"  "refs/tags/v1.2.3"   "v1.2.3"  "v1.2.3"        "true"
check_output "a testnet push drops the transparency log" "refs/heads/testnet" "testnet" "testnet-latest" "false"
check_output "a main push drops the transparency log"    "refs/heads/main"    "main"    "main-latest"    "false"

if ! printf 'sha_tag=sha-%s\n' "${FULL_SHA:0:7}" | diff -q - <("$CHECK" "refs/heads/testnet" "testnet" "$FULL_SHA" | head -1) >/dev/null; then
  echo "FAIL: sha_tag is not the first 7 characters of the full sha"
  fail=1
else
  echo "ok: sha_tag is the first 7 characters of the full sha"
fi

"$CHECK" "" "testnet" "$FULL_SHA" >/dev/null 2>&1
if [ $? -eq 0 ]; then echo "FAIL: an empty ref is accepted"; fail=1; else echo "ok: an empty ref is refused"; fi

"$CHECK" "refs/heads/testnet" "testnet" "short" >/dev/null 2>&1
if [ $? -eq 0 ]; then echo "FAIL: a too-short sha is accepted"; fail=1; else echo "ok: a too-short sha is refused"; fi

if [ "$fail" -ne 0 ]; then
  echo "compute-image-tags.sh does NOT behave as documented"; exit 1
fi
echo "compute-image-tags.sh behaves as documented"
