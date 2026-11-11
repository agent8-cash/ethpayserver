#!/usr/bin/env bash
# Proves commons.sh can find crates in the manifest with the correct URL.
# Before the fix, commons.sh had the wrong URL (randomcash instead of agent8-cash),
# so it would die at line 36 when trying to run any command.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guard="$repo_root/scripts/commons.sh"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

fixture="$workdir/fixture"
mkdir -p "$fixture/scripts"
cp "$guard" "$fixture/scripts/commons.sh"

pass=0
fail=0

run_commons_status() {
  cd "$fixture"
  "$fixture/scripts/commons.sh" status >"$workdir/out" 2>&1
}

write_manifest() {
  local url="$1"
  shift
  printf '[workspace]\nresolvers = ["3"]\n\n' > "$fixture/Cargo.toml"
  printf '[workspace.package]\n\n' >> "$fixture/Cargo.toml"
  printf '[workspace.dependencies]\n' >> "$fixture/Cargo.toml"
  for crate in "$@"; do
    printf '%s = { git = "%s", rev = "0000000000000000000000000000000000000000" }\n' \
      "$crate" "$url" >> "$fixture/Cargo.toml"
  done
}

assert_succeeds_with_url() {
  local desc="$1" url="$2"
  shift 2
  write_manifest "$url" "$@"
  local rc=0
  run_commons_status || rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "ok: $desc"
    pass=$((pass + 1))
  else
    echo "FAIL: $desc - wanted exit 0, got $rc"
    sed 's/^/  /' "$workdir/out"
    fail=$((fail + 1))
  fi
}

assert_fails_with_url() {
  local desc="$1" url="$2"
  shift 2
  write_manifest "$url" "$@"
  local rc=0
  run_commons_status || rc=$?
  if [ "$rc" -ne 0 ]; then
    if grep -q "no commons crates found" "$workdir/out"; then
      echo "ok: $desc"
      pass=$((pass + 1))
    else
      echo "FAIL: $desc - expected 'no commons crates found', got:"
      sed 's/^/  /' "$workdir/out"
      fail=$((fail + 1))
    fi
  else
    echo "FAIL: $desc - wanted exit non-zero, got 0"
    sed 's/^/  /' "$workdir/out"
    fail=$((fail + 1))
  fi
}

# Test with correct URL - should find crates and run successfully
assert_succeeds_with_url "commons.sh finds crates with correct agent8-cash URL" \
  "https://github.com/agent8-cash/payserver-commons.git" \
  "api-types" "scrub" "types" "auth"

# Test with wrong URL - should fail to find crates
assert_fails_with_url "commons.sh fails with wrong randomcash URL" \
  "https://github.com/randomcash/payserver-commons.git" \
  "api-types" "scrub" "types"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
