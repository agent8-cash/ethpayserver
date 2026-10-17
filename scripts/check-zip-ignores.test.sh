#!/usr/bin/env bash
# Proves check-zip-ignores.sh goes red on the cases it claims to, and stays
# green on the one it must not trip. A guard that only ever passed proves nothing.
set -uo pipefail
GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-zip-ignores.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
fail=0

lock() { printf '[[package]]\nname = "zip"\nversion = "%s"\n' "$1" > "$TMP/Cargo.lock"; }
audit() { printf '[advisories]\nignore = [\n%s]\n' "$1" > "$TMP/audit.toml"; }
edges() { printf '%s' "$1" > "$TMP/edges"; }
ALL='    "RUSTSEC-2026-0367",
    "RUSTSEC-2026-0368",
    "RUSTSEC-2026-0369",
'

check() { # name want-rc
  ZIP_EDGES_FILE="$TMP/edges" LOCK_FILE="$TMP/Cargo.lock" AUDIT_FILE="$TMP/audit.toml" "$GUARD" >/dev/null 2>&1
  got=$?
  if [ "$got" -ne "$2" ]; then echo "FAIL: $1 - expected exit $2, got $got"; fail=1; else echo "ok: $1 (exit $got)"; fi
}

lock 3.0.0; audit "$ALL"; edges 'utoipa-swagger-ui build
'
check "the tree as it is today passes" 0

edges 'utoipa-swagger-ui normal
'
check "a normal edge to zip is refused" 1

edges 'utoipa-swagger-ui dev
'
check "a dev edge to zip is refused" 1

edges 'some-other-crate build
'
check "a build edge from a dependent the guard does not name is refused" 1

edges 'utoipa-swagger-ui build
some-other-crate build
'
check "one unnamed dependent among named ones is refused" 1

edges ''
check "no edge to zip at all is 'could not run', not a pass" 2

edges 'utoipa-swagger-ui build
'
lock 8.9.9; check "an affected release with the ignores passes" 0
lock 9.0.0; check "the fixed release with the ignores still listed is refused" 1
lock 9.1.0; check "a later fixed release with the ignores still listed is refused" 1
audit ''; check "the fixed release with the ignores removed passes" 0
lock 3.0.0; audit ''; check "an affected release with no ignores passes (the guard does not require them)" 0

exit $fail
