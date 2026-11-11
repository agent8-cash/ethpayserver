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
# RUSTSEC-2099-0001 (shipped, reachable)      -> should pass
# RUSTSEC-2099-0002 (not in the fixture)      -> "fixed upstream", should fail
# RUSTSEC-2099-0003 (otherplatform)           -> matched but unreachable, should fail
# RUSTSEC-2099-0004 (shipped, reachable)      -> should pass (used for two-ID tests)
# RUSTSEC-2026-0367 (otherplatform, zip's id) -> has its own guard, must be skipped
printf '{"vulnerabilities":{"list":[%s]},"warnings":{"unmaintained":[%s],"unsound":[],"yanked":[]}}' \
  "$(entry RUSTSEC-2099-0001 shipped),$(entry RUSTSEC-2099-0003 otherplatform),$(entry RUSTSEC-2099-0004 shipped),$(entry RUSTSEC-2026-0367 otherplatform)" \
  "" > "$TMP/unfiltered.json"

run() { UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD"; }
check() { # name want-rc
  out="$(run 2>&1)"; got=$?
  if [ "$got" -ne "$2" ]; then echo "FAIL: $1 - expected exit $2, got $got"; echo "$out"; fail=1; else echo "ok: $1 (exit $got)"; fi
  printf '%s' "$out"
}

audit '    # EXPIRES 2026-11-25
    "RUSTSEC-2099-0001",
'
out=$(check "a still-open, reachable ignore with future EXPIRES passes alone" 0)
case "$out" in *"OK    RUSTSEC-2099-0001  shipped 1.0.0"*) echo "ok: and it is labelled OK";; *) echo "FAIL: missing OK label"; echo "$out"; fail=1;; esac

audit '    "RUSTSEC-2099-0002",
'
out=$(check "an advisory cargo audit no longer finds fails" 1)
case "$out" in *"DEAD  RUSTSEC-2099-0002"*) echo "ok: and it is labelled DEAD";; *) echo "FAIL: missing DEAD label"; echo "$out"; fail=1;; esac

audit '    "RUSTSEC-2099-0003",
'
out=$(check "an advisory matched but not reachable (lockfile only) fails" 1)
case "$out" in *"DEAD  RUSTSEC-2099-0003  otherplatform 1.0.0"*"lockfile only"*) echo "ok: and it names lockfile-only";; *) echo "FAIL: missing lockfile-only label"; echo "$out"; fail=1;; esac

audit '    # EXPIRES 2026-11-10
    "RUSTSEC-2026-0367",
'
out=$(AUDIT_TODAY="2026-11-11" AUDIT_TODAY_OK=1 UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 1 ]; then echo "ok: an id with its own guard but expired EXPIRES fails (SKIP_IDS are still expiry-checked)"
else echo "FAIL: expired SKIP_ID should fail, got exit $rc"; echo "$out"; fail=1; fi

audit '    "RUSTSEC-2099-0001",
    "RUSTSEC-2099-0003",
'
check "one dead id among passing ones still fails the whole run" 1

# EXPIRES date edge cases
audit '    # EXPIRES 2026-11-11
    "RUSTSEC-2099-0001",
'
out=$(AUDIT_TODAY="2026-11-11" AUDIT_TODAY_OK=1 UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 0 ]; then echo "ok: EXPIRES equal to today passes (boundary is exclusive)"
else echo "FAIL: EXPIRES = today should pass, got exit $rc"; echo "$out"; fail=1; fi

audit '    # EXPIRES 2026-11-10
    "RUSTSEC-2099-0001",
'
out=$(AUDIT_TODAY="2026-11-11" AUDIT_TODAY_OK=1 UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 1 ]; then echo "ok: EXPIRES in the past fails"
  case "$out" in *"EXPIRED  RUSTSEC-2099-0001"*) echo "ok: and names the expired ID";; *) echo "FAIL: missing EXPIRED line"; echo "$out"; fail=1;; esac
else echo "FAIL: past EXPIRES should fail, got exit $rc"; echo "$out"; fail=1; fi

# Two IDs sharing one EXPIRES comment block - both should inherit the block's expiry
audit '    # EXPIRES 2026-11-10
    "RUSTSEC-2099-0001",
    "RUSTSEC-2099-0004",
'
out=$(AUDIT_TODAY="2026-11-11" AUDIT_TODAY_OK=1 UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 1 ]; then echo "ok: two IDs under one expired EXPIRES block both fail"
  has_0001=$(echo "$out" | grep -c "EXPIRED  RUSTSEC-2099-0001") || true
  has_0004=$(echo "$out" | grep -c "EXPIRED  RUSTSEC-2099-0004") || true
  if [ "$has_0001" -gt 0 ] && [ "$has_0004" -gt 0 ]; then echo "ok: and names both IDs"
  else echo "FAIL: missing EXPIRED lines for both IDs"; echo "$out"; fail=1; fi
else echo "FAIL: two expired IDs should fail, got exit $rc"; echo "$out"; fail=1; fi

# Two IDs with future EXPIRES - should pass
audit '    # EXPIRES 2026-11-25
    "RUSTSEC-2099-0001",
    "RUSTSEC-2099-0004",
'
out=$(AUDIT_TODAY="2026-11-11" AUDIT_TODAY_OK=1 UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 0 ]; then echo "ok: two IDs under one future EXPIRES block pass"
else echo "FAIL: two future-expiry IDs should pass, got exit $rc"; echo "$out"; fail=1; fi

# No EXPIRES date - should pass (not failed for missing expiry)
audit '    "RUSTSEC-2099-0001",
'
out=$(AUDIT_TODAY="2026-11-11" AUDIT_TODAY_OK=1 UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 0 ]; then echo "ok: no EXPIRES date passes (not failed for that reason)"
else echo "FAIL: ignore with no EXPIRES should pass, got exit $rc"; echo "$out"; fail=1; fi

# AUDIT_TODAY without AUDIT_TODAY_OK=1 should fail with exit 2
audit '    # EXPIRES 2026-11-25
    "RUSTSEC-2099-0001",
'
out=$(AUDIT_TODAY="2026-11-11" UNFILTERED_JSON_FILE="$TMP/unfiltered.json" UNFILTERED_FIXTURE_OK=1 AUDIT_FILE="$TMP/audit.toml" "$GUARD" 2>&1); rc=$?
if [ "$rc" -eq 2 ]; then echo "ok: AUDIT_TODAY without AUDIT_TODAY_OK=1 exits 2 (COULD NOT RUN)"
  case "$out" in *"refusing to use"*) echo "ok: and names the guard";; *) echo "FAIL: refusal does not name the guard"; echo "$out"; fail=1;; esac
else echo "FAIL: AUDIT_TODAY without OK should exit 2, got $rc"; echo "$out"; fail=1; fi

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
