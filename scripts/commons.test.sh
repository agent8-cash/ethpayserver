#!/usr/bin/env bash
# Self-test for scripts/commons.sh. Each case runs the script itself, not a
# re-implementation of it. Cases 1-3 run a byte-for-byte copy of the script
# inside a throwaway workspace whose manifest pins commons crates to a local
# file:// URL, because the script reads its manifest relative to its own
# location. Case 4 runs the link against a copy of the repository's real
# manifest. Expected crates and URL are always read back from the manifest
# under test, never copied into this file.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
commons_sh="$repo_root/scripts/commons.sh"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

pass=0
fail=0
assert() { # assert <description> <command...>
  local desc="$1"; shift
  if "$@"; then
    pass=$((pass + 1))
  else
    echo "FAIL: $desc"
    fail=$((fail + 1))
  fi
}

# Names of the commons crates a manifest pins, and the commons URL it uses.
manifest_crates() { grep -oP '^\K[a-z0-9_-]+(?= *= *\{ *git *=)' "$1" | sort -u; }
manifest_url() { grep -m1 -oP 'git = "\K[^"]*payserver-commons\.git' "$1"; }

# Input for the fixture manifest only; every expectation below is read back
# from the manifest the script actually sees.
fixture_crates=(api-types auth crypto payserver-plugin-api payserver-plugin-host rates scrub types)
fixture_url="file://$workdir/remote/payserver-commons.git"

# make_workspace <dir> <manifest-text>: a workspace holding a copy of the script.
make_workspace() {
  mkdir -p "$1/scripts"
  cp "$commons_sh" "$1/scripts/commons.sh"
  chmod +x "$1/scripts/commons.sh"
  printf '%s\n' "$2" > "$1/Cargo.toml"
}

pinned_manifest() {
  echo '[workspace]'
  echo 'members = []'
  echo
  echo '[workspace.dependencies]'
  for c in "${fixture_crates[@]}"; do
    echo "$c = { git = \"$fixture_url\", rev = \"6382134022d6761cf2be877730b42aa67a1de39e\" }"
  done
}

# Fixture manifest on disk, so its crates and URL can be read back from it.
fixture_manifest="$workdir/fixture.toml"
pinned_manifest > "$fixture_manifest"

# make_commons <dir> <manifest>: a commons checkout with a directory per crate
# the manifest pins, as link expects.
make_commons() {
  local c
  for c in $(manifest_crates "$2"); do mkdir -p "$1/$c"; done
}

commons="$workdir/commons"
make_commons "$commons" "$fixture_manifest"

# Case 1: link writes a patch header byte-equal to the manifest URL, patches
# exactly the manifest's crates, and status then reports the link ACTIVE.
ws1="$workdir/ws1"
make_workspace "$ws1" "$(cat "$fixture_manifest")"
rc=0
(cd "$ws1" && scripts/commons.sh link "$commons" > "$workdir/link1.txt" 2>&1) || rc=$?
assert "case 1: link exits 0 against a manifest pinned to a file URL (got $rc)" test "$rc" -eq 0
cfg1="$ws1/.cargo/config.toml"
assert "case 1: .cargo/config.toml was written" test -f "$cfg1"
if [ -f "$cfg1" ]; then
  header="$(grep -m1 '^\[patch\.' "$cfg1" || true)"
  assert "case 1: patch header is byte-equal to the manifest URL" \
    test "$header" = "[patch.\"$(manifest_url "$fixture_manifest")\"]"
  for c in $(manifest_crates "$fixture_manifest"); do
    assert "case 1: crate $c is patched" grep -q "^$c = { path = " "$cfg1"
  done
  patched_count="$(grep -c '= { path = ' "$cfg1" || true)"
  manifest_count="$(manifest_crates "$fixture_manifest" | wc -l)"
  assert "case 1: patches exactly the manifest's crates (got $patched_count, manifest has $manifest_count)" \
    test "$patched_count" -eq "$manifest_count"
fi
status1="$(cd "$ws1" && scripts/commons.sh status 2>&1 || true)"
assert "case 1: status reports the link ACTIVE after link" \
  grep -q '^local link : ACTIVE' <(printf '%s\n' "$status1")

# Case 2: a manifest with no commons dependency fails loudly.
ws2="$workdir/ws2"
make_workspace "$ws2" "$(printf '[dependencies]\nserde = "1"\n')"
rc=0
out2="$(cd "$ws2" && scripts/commons.sh link "$commons" 2>&1)" || rc=$?
assert "case 2: link exits non-zero when no commons crate is pinned (got $rc)" test "$rc" -ne 0
assert "case 2: diagnostic names the missing crates" \
  grep -q 'no commons crates found' <(printf '%s\n' "$out2")

# Case 3: a patch table for a URL the manifest does not use is reported STALE,
# not ACTIVE.
ws3="$workdir/ws3"
make_workspace "$ws3" "$(cat "$fixture_manifest")"
mkdir -p "$ws3/.cargo"
printf '[patch."https://example.invalid/other/payserver-commons.git"]\n' > "$ws3/.cargo/config.toml"
out3="$(cd "$ws3" && scripts/commons.sh status 2>&1)"
assert "case 3: status does not report a non-matching patch as ACTIVE" \
  bash -c '! grep -q "ACTIVE" <<<"$1"' _ "$out3"
assert "case 3: status reports the non-matching patch as STALE" \
  grep -q 'STALE' <(printf '%s\n' "$out3")

# Case 4: against the repository's real manifest (a copy, so the repository's
# own .cargo is never written), link patches the real URL and real crate set,
# and status reports the real pin.
real_manifest="$repo_root/Cargo.toml"
ws4="$workdir/ws4"
make_workspace "$ws4" "$(cat "$real_manifest")"
real_commons="$workdir/real-commons"
make_commons "$real_commons" "$real_manifest"
rc=0
(cd "$ws4" && scripts/commons.sh link "$real_commons" > "$workdir/link4.txt" 2>&1) || rc=$?
assert "case 4: link exits 0 against the real manifest (got $rc)" test "$rc" -eq 0
cfg4="$ws4/.cargo/config.toml"
if [ -f "$cfg4" ]; then
  real_header="$(grep -m1 '^\[patch\.' "$cfg4" || true)"
  assert "case 4: patch header is the real manifest URL" \
    test "$real_header" = "[patch.\"$(manifest_url "$real_manifest")\"]"
  for c in $(manifest_crates "$real_manifest"); do
    assert "case 4: real crate $c is patched" grep -q "^$c = { path = " "$cfg4"
  done
else
  assert "case 4: .cargo/config.toml was written" false
fi

rc=0
out4="$("$commons_sh" status 2>&1)" || rc=$?
assert "case 4: status exits 0 against the repository (got $rc)" test "$rc" -eq 0
real_rev="$(grep -m1 -oP 'rev = "\K[0-9a-f]{40}' "$real_manifest")"
assert "case 4: status prints the pinned rev from Cargo.toml" \
  grep -qx "pinned rev : $real_rev" <(printf '%s\n' "$out4")

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
