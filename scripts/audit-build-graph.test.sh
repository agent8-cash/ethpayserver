#!/usr/bin/env bash
# Proves audit-build-graph.sh blocks on an advisory in a crate we build, and
# does not on one that only the lockfile mentions. A gate that only ever passed
# proves nothing.
set -uo pipefail
GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/audit-build-graph.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
# A real fixture workspace, so the answer comes from real `cargo tree` and not
# from a mock that agrees with whatever the script assumes: `shipped` is a normal
# dependency, `devonly` and `featureonly` enter by a dev edge and an optional
# feature (both must block), `otherplatform` enters only through a target that is not ours.
# (Path crates under the root are implicitly members, hence the exclude.)
mkdir -p "$TMP/fx/app/src" "$TMP/fx/shipped/src" "$TMP/fx/otherplatform/src" "$TMP/fx/devonly/src" "$TMP/fx/featureonly/src"
cat > "$TMP/fx/Cargo.toml" <<'EOS'
[workspace]
members = ["app"]
exclude = ["shipped", "otherplatform", "devonly", "featureonly"]
resolver = "3"
EOS
cat > "$TMP/fx/app/Cargo.toml" <<'EOS'
[package]
name = "app"
version = "1.0.0"
edition = "2021"
[dependencies]
shipped = { path = "../shipped" }
featureonly = { path = "../featureonly", optional = true }
[dev-dependencies]
devonly = { path = "../devonly" }
[target.'cfg(target_os = "nonexistent")'.dependencies]
otherplatform = { path = "../otherplatform" }
EOS
for c in shipped otherplatform devonly featureonly; do
  printf '[package]\nname = "%s"\nversion = "1.0.0"\nedition = "2021"\n' "$c" > "$TMP/fx/$c/Cargo.toml"
  : > "$TMP/fx/$c/src/lib.rs"
done
: > "$TMP/fx/app/src/lib.rs"
(cd "$TMP/fx" && cargo generate-lockfile --offline -q) || { echo "FAIL: could not build the fixture"; exit 1; }
cd "$TMP/fx"
fail=0
vuln() { printf '{"vulnerabilities":{"list":[%s]}}' "$1" > "$TMP/audit.json"; }
entry() { printf '{"advisory":{"id":"%s"},"package":{"name":"%s","version":"1.0.0"}}' "$1" "$2"; }
check() { # name want-rc
  AUDIT_JSON_FILE="$TMP/audit.json" AUDIT_FIXTURE_OK=1 "$GUARD" >"$TMP/out" 2>&1
  got=$?
  if [ "$got" -ne "$2" ]; then echo "FAIL: $1 - expected exit $2, got $got"; cat "$TMP/out"; fail=1; else echo "ok: $1 (exit $got)"; fi
}

# THE FIXTURE MARKER, BOTH HALVES.
#
# AUDIT_JSON_FILE alone must not substitute a fixture for the real scan: anything
# that sets it in the environment would otherwise replace the advisory scan with a
# file and report a pass. And the legitimate path must still work, or the guard
# would break its own test and get removed.
marker_cases() {
    vuln ""
    # subject: the fixture is named but not declared -> could-not-run, exit 2.
    out=$(AUDIT_JSON_FILE="$TMP/audit.json" "$GUARD" 2>&1); rc=$?
    if [ "$rc" -eq 2 ]; then echo "ok: an undeclared fixture is could-not-run (exit 2)"
    else echo "FAIL: undeclared fixture exited $rc, wanted 2"; echo "$out"; fail=1; fi
    case "$out" in
        *"refusing to substitute"*AUDIT_FIXTURE_OK*|*AUDIT_FIXTURE_OK*"refusing to substitute"*)
            echo "ok: and the REFUSAL names the marker it wanted" ;;
        *) echo "FAIL: the refusal does not name AUDIT_FIXTURE_OK (matching the marker alone also matches the success note)"; echo "$out"; fail=1 ;;
    esac
    # control: declared -> the fixture is used and an empty report passes. Without
    # this the subject above would pass even if the marker blocked EVERYTHING.
    out=$(AUDIT_JSON_FILE="$TMP/audit.json" AUDIT_FIXTURE_OK=1 "$GUARD" 2>&1); rc=$?
    if [ "$rc" -eq 0 ]; then echo "ok: a declared fixture is used (exit 0)"
    else echo "FAIL: declared fixture exited $rc, wanted 0"; echo "$out"; fail=1; fi
    # and it says it substituted, so a fixture run is never mistaken for a real scan
    case "$out" in
        *"using the fixture"*) echo "ok: and a fixture run announces itself" ;;
        *) echo "FAIL: a fixture run does not announce itself"; echo "$out"; fail=1 ;;
    esac
}
marker_cases

vuln ""; check "no advisories passes" 0
vuln "$(entry RUSTSEC-X otherplatform)"; check "advisory in a lockfile-only crate does not block" 0
grep -q "LOCKFILE ONLY" "$TMP/out" || { echo "FAIL: lockfile-only case is not labelled in the output"; fail=1; }
vuln "$(entry RUSTSEC-Y shipped)"; check "advisory in a crate we build blocks" 1
grep -q "IN BUILD GRAPH" "$TMP/out" || { echo "FAIL: build-graph case is not labelled in the output"; fail=1; }
vuln "$(entry RUSTSEC-X otherplatform),$(entry RUSTSEC-Y shipped)"; check "one built crate among lockfile-only ones blocks" 1
vuln "$(entry RUSTSEC-D devonly)"; check "a dev-only crate blocks (--edges all)" 1
vuln "$(entry RUSTSEC-F featureonly)"; check "an optional-feature crate blocks (--all-features)" 1
grep -q "IN BUILD GRAPH" "$TMP/out" || { echo "FAIL: feature-only crate is not classified as in the build graph"; fail=1; }
vuln "$(entry RUSTSEC-X notinlockfile)"; check "a cargo tree error fails closed" 1
grep -q "COULD NOT CLASSIFY" "$TMP/out" || { echo "FAIL: tree error is not labelled"; fail=1; }
vuln '{"advisory":{"id":"RUSTSEC-Z"},"package":{"name":"otherplatform"}}'; check "an entry with no version fails closed" 1
grep -q "COULD NOT CLASSIFY an advisory entry" "$TMP/out" || { echo "FAIL: missing-version guard did not fire"; fail=1; }
printf '{"vulnerabilities":{"list":["x"]}}' > "$TMP/audit.json"; check "a non-object entry is 'could not run', not a pass" 2
BUILD_TARGET=all vuln "$(entry RUSTSEC-X otherplatform)"; BUILD_TARGET=all check "--target all would put the other platform's crate in scope (why the script must not use it)" 1
echo 'not json' > "$TMP/audit.json"; check "unparseable audit output is 'could not run', not a pass" 2
exit $fail
