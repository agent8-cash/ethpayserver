#!/usr/bin/env bash
# Proves check-advisory-ignores.sh blocks on an ignore that no longer applies
# (not found once un-ignored, or found but not reachable) and does not block
# on one that is still open in a crate we actually build. A gate that only
# ever passed proves nothing.
set -uo pipefail
GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-advisory-ignores.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# A real fixture workspace, so "reachable" comes from real `cargo tree` and not
# a mock that agrees with whatever the script assumes: `shipped` is a normal
# dependency (reachable); `otherplatform` enters only through a target that is
# not ours, so it is lockfile-only (unreachable) on the target this test uses.
mkdir -p "$TMP/fx/app/src" "$TMP/fx/shipped/src" "$TMP/fx/otherplatform/src"
cat > "$TMP/fx/Cargo.toml" <<'EOS'
[workspace]
members = ["app"]
exclude = ["shipped", "otherplatform"]
resolver = "3"
EOS
cat > "$TMP/fx/app/Cargo.toml" <<'EOS'
[package]
name = "app"
version = "1.0.0"
edition = "2021"
[dependencies]
shipped = { path = "../shipped" }
[target.'cfg(target_os = "nonexistent")'.dependencies]
otherplatform = { path = "../otherplatform" }
EOS
for c in shipped otherplatform; do
  printf '[package]\nname = "%s"\nversion = "1.0.0"\nedition = "2021"\n' "$c" > "$TMP/fx/$c/Cargo.toml"
  : > "$TMP/fx/$c/src/lib.rs"
done
: > "$TMP/fx/app/src/lib.rs"
(cd "$TMP/fx" && cargo generate-lockfile --offline -q) || { echo "FAIL: could not build the fixture"; exit 1; }
cd "$TMP/fx"

fail=0

audit() { printf '[advisories]\nignore = [\n%s]\n' "$1" > "$TMP/audit.toml"; }
entry() { printf '{"advisory":{"id":"%s"},"package":{"name":"%s","version":"1.0.0"}}' "$1" "$2"; }
# RUSTSEC-2099-0001 (shipped, reachable)   -> should pass
# RUSTSEC-2099-0002 (not in the fixture)   -> "fixed upstream", should fail
# RUSTSEC-2099-0003 (otherplatform)        -> matched but unreachable, should fail
# RUSTSEC-2026-0367 (otherplatform, zip's id) -> has its own guard, must be skipped
printf '{"vulnerabilities":{"list":[%s]},"warnings":{"unmaintained":[%s],"unsound":[],"yanked":[]}}' \
  "$(entry RUSTSEC-2099-0001 shipped),$(entry RUSTSEC-2099-0003 otherplatform),$(entry RUSTSEC-2026-0367 otherplatform)" \
  "" > "$TMP/unfiltered.json"

run() { UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD"; }
check() { # name want-rc
  out="$(run 2>&1)"; got=$?
  if [ "$got" -ne "$2" ]; then echo "FAIL: $1 - expected exit $2, got $got"; echo "$out"; fail=1; else echo "ok: $1 (exit $got)"; fi
  printf '%s' "$out"
}

audit '    "RUSTSEC-2099-0001",
'
out=$(check "a still-open, reachable ignore passes alone" 0)
case "$out" in *"OK    RUSTSEC-2099-0001  shipped 1.0.0"*) echo "ok: and it is labelled OK";; *) echo "FAIL: missing OK label"; echo "$out"; fail=1;; esac

audit '    "RUSTSEC-2099-0002",
'
out=$(check "an advisory cargo audit no longer finds fails" 1)
case "$out" in *"DEAD  RUSTSEC-2099-0002"*) echo "ok: and it is labelled DEAD";; *) echo "FAIL: missing DEAD label"; echo "$out"; fail=1;; esac

audit '    "RUSTSEC-2099-0003",
'
out=$(check "an advisory matched but not reachable (lockfile only) fails" 1)
case "$out" in *"DEAD  RUSTSEC-2099-0003  otherplatform 1.0.0"*"lockfile only"*) echo "ok: and it names lockfile-only";; *) echo "FAIL: missing lockfile-only label"; echo "$out"; fail=1;; esac

audit '    "RUSTSEC-2026-0367",
'
check "an id with its own guard is skipped, not evaluated as dead" 0

audit '    "RUSTSEC-2099-0001",
    "RUSTSEC-2099-0003",
'
check "one dead id among passing ones still fails the whole run" 1

# fixture-marker safety, both halves - see audit-build-graph.sh for why an
# env var alone must never silently substitute a fixture for the real scan.
audit '    "RUSTSEC-2099-0001",
'
out=$(UNFILTERED_JSON_FILE="$TMP/unfiltered.json" AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 2 ]; then echo "ok: an undeclared fixture is could-not-run (exit 2)"
else echo "FAIL: undeclared fixture exited $rc, wanted 2"; echo "$out"; fail=1; fi
case "$out" in *"refusing to substitute"*) echo "ok: and the refusal names the marker";; *) echo "FAIL: refusal does not name the marker"; echo "$out"; fail=1;; esac

printf 'not json' > "$TMP/bad.json"
out=$(UNFILTERED_JSON_FILE="$TMP/bad.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 2 ]; then echo "ok: unparseable unfiltered output is could-not-run (exit 2)"
else echo "FAIL: unparseable output exited $rc, wanted 2"; echo "$out"; fail=1; fi

exit $fail
