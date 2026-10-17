#!/usr/bin/env bash
# Guards the three zip advisories ignored in .cargo/audit.toml.
#
# zip is in this tree for one reason: utoipa-swagger-ui unpacks the Swagger UI
# dist that ships inside the crate, from its build script. All three advisories
# are in extraction - symlink targets and permission bits - so each describes a
# risk to whoever extracts an archive they did not author. Nothing here does:
# the only archive extracted is the one vendored in that crate, at build time,
# and no runtime path reaches zip.
#
# Both halves of that are properties of the build today, not laws:
#
#   1. If zip becomes reachable by a normal or dev edge - a new dependency, or
#      utoipa-swagger-ui changing how it ships its assets - the ignores would
#      cover a real exposure while the comment beside them still read as true.
#      So this fails on any edge to zip that is not a build edge, and on any
#      dependent this file does not name.
#   2. They are a stopgap for a version that is not fixed yet. Once the locked
#      zip is >= 9.0.0 the advisories no longer apply and the ignores are dead
#      weight: an allowlist with no end is how a stopgap becomes permanent. So
#      this fails if the lock has reached a fixed release while any remains.
#
# Test hooks (used only by check-zip-ignores.test.sh): ZIP_EDGES_FILE replaces
# the `cargo metadata` output as "<dependent> <kind>" lines, LOCK_FILE and
# AUDIT_FILE replace the paths.
set -uo pipefail

LOCK="${LOCK_FILE:-Cargo.lock}"
AUDIT="${AUDIT_FILE:-.cargo/audit.toml}"
IDS="RUSTSEC-2026-0367 RUSTSEC-2026-0368 RUSTSEC-2026-0369"
FIXED=9.0.0
# The dependents allowed to reach zip, and only through a build edge. A closed
# set on purpose: a new one has to be added here, on a pull request, with the
# reasoning above re-checked against it.
ALLOWED_DEPENDENTS="utoipa-swagger-ui"

if [ -n "${ZIP_EDGES_FILE:-}" ]; then
  edges="$(cat "$ZIP_EDGES_FILE")"
else
  meta="$(cargo metadata --locked --format-version 1 2>&1)" || {
    echo "cargo metadata failed:"; echo "$meta"; exit 2; }
  edges="$(jq -r '.packages[] | .name as $p | .dependencies[]?
                  | select(.name == "zip") | "\($p) \(.kind // "normal")"' <<<"$meta")" || {
    echo "could not read zip edges from cargo metadata - not a pass"; exit 2; }
fi

if [ -z "${edges//[[:space:]]/}" ]; then
  echo "ERROR: no edge to zip found at all - the guard could not measure what it claims (not a pass)."
  echo "If zip has genuinely left the tree, remove the ignores and this guard together."
  exit 2
fi

rc=0
while read -r dep kind; do
  [ -z "$dep" ] && continue
  if [ "$kind" != "build" ]; then
    echo "FAIL: $dep reaches zip by a $kind edge, not a build edge."
    echo "The ignores in $AUDIT assume zip is only ever run by a build script on a vendored archive."
    rc=1
  fi
  case " $ALLOWED_DEPENDENTS " in
    *" $dep "*) ;;
    *) echo "FAIL: $dep depends on zip and is not named in this guard."
       echo "Re-check the extraction reasoning for $dep, then add it to ALLOWED_DEPENDENTS."
       rc=1 ;;
  esac
done <<<"$edges"

ver="$(awk '/^name = "zip"$/ {getline; gsub(/[^0-9.]/, "", $0); print; exit}' "$LOCK")"
if [ -z "$ver" ]; then
  echo "FAIL: zip not found in $LOCK"
  rc=1
else
  ge() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$2" ]; }
  if ge "$ver" "$FIXED"; then
    left="$(for id in $IDS; do grep -q "\"$id\"" "$AUDIT" && printf '%s ' "$id"; done)"
    if [ -n "$left" ]; then
      echo "FAIL: zip $ver is a fixed release but $AUDIT still ignores: $left"
      echo "Remove those ignores; they only hide what the fix already closed."
      rc=1
    fi
  fi
fi

[ "$rc" = 0 ] && echo "ok: zip ${ver:-?} is reached only by a build edge from$(printf ' %s' $ALLOWED_DEPENDENTS), so the ignores still hold"
exit "$rc"
