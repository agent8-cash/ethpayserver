#!/usr/bin/env bash
# Every advisory ignored in .cargo/audit.toml must keep being true in one of
# two ways, or the ignore is dead weight: an allowlist with no end is how a
# stopgap becomes permanent.
#
#   1. cargo audit, run WITHOUT the ignore list, still matches the advisory
#      against our locked version - it has not been fixed upstream, and the
#      crate it names is a real (registry-sourced) dependency, not merely a
#      same-named path crate cargo audit happens to never match.
#   2. the crate cargo audit actually matched is reachable from a workspace
#      member, on the target we build for, over any edge and any feature -
#      the identical question scripts/audit-build-graph.sh answers for
#      advisories cargo audit reports un-ignored, via the identical
#      `cargo tree -i <crate>@<ver> --edges all --all-features` call. Reused
#      here rather than reimplemented.
#
# Either property failing means the ignore protects nothing today: fixed
# upstream, or never compiled. That is a FAIL, not a warning - the entry
# should be removed from .cargo/audit.toml.
#
# RUSTSEC-2026-0315/0316/0325/0326/0327 (wasmtime) and RUSTSEC-2026-0367/
# 0368/0369 (zip) have their own guards (check-wasmtime-ignores.sh,
# check-zip-ignores.sh) that additionally watch narrower assumptions - banned
# features off, edge kind, a closed dependent allowlist - this script does
# not know about. Skipped here so the two checks never do the same job twice.
#
# Test hooks (used only by check-advisory-ignores.test.sh): UNFILTERED_JSON_FILE
# replaces the un-ignored `cargo audit --json` run (requires
# UNFILTERED_FIXTURE_OK=1 - see audit-build-graph.sh for why an env var alone
# must not silently substitute a fixture for the real scan), AUDIT_FILE
# replaces .cargo/audit.toml, BUILD_TARGET matches audit-build-graph.sh's.
set -uo pipefail

AUDIT="${AUDIT_FILE:-.cargo/audit.toml}"
target="${BUILD_TARGET:-x86_64-unknown-linux-gnu}"
# ids with their own dedicated guard - see header.
SKIP_IDS=" RUSTSEC-2026-0315 RUSTSEC-2026-0316 RUSTSEC-2026-0325 RUSTSEC-2026-0326 RUSTSEC-2026-0327 RUSTSEC-2026-0367 RUSTSEC-2026-0368 RUSTSEC-2026-0369 "

report=$(mktemp); trap 'rm -f "$report"' EXIT

if [ -n "${UNFILTERED_JSON_FILE:-}" ]; then
  if [ "${UNFILTERED_FIXTURE_OK:-}" != 1 ]; then
    echo "ERROR: UNFILTERED_JSON_FILE is set but UNFILTERED_FIXTURE_OK is not 1 - refusing to substitute a fixture for the real scan. COULD NOT RUN (not a pass)." >&2
    exit 2
  fi
  echo "NOTE: using the fixture at $UNFILTERED_JSON_FILE instead of cargo audit (UNFILTERED_FIXTURE_OK=1)" >&2
  cp "$UNFILTERED_JSON_FILE" "$report"
else
  # cargo audit reads .cargo/audit.toml's ignore list from the CURRENT
  # DIRECTORY, not from the lockfile's location or from a --file path - so run
  # it from a scratch directory with an empty ignore list to get back every
  # advisory our locked versions actually match, ignored or not.
  # --no-fetch: the "cargo audit (build graph blocks, lockfile-only reports)"
  # step earlier in this job already fetched the same database.
  lockfile="$(cd "$(dirname "$AUDIT")/.." && pwd)/Cargo.lock"
  scratch="$(mktemp -d)"; mkdir -p "$scratch/.cargo"
  printf '[advisories]\nignore = []\n' > "$scratch/.cargo/audit.toml"
  (cd "$scratch" && cargo audit --json --no-fetch -f "$lockfile" > "$report" 2>/dev/null)
  rm -rf "$scratch"
fi

if ! jq -e '.vulnerabilities.list | type == "array"' "$report" >/dev/null 2>&1; then
  echo "ERROR: the unfiltered cargo audit report did not parse - COULD NOT RUN (not a pass)."
  head -c 2000 "$report"
  exit 2
fi

ids="$(grep -oE '"RUSTSEC-[0-9]+-[0-9]+"' "$AUDIT" | tr -d '"' | sort -u)"
dead=0
for id in $ids; do
  case "$SKIP_IDS" in *" $id "*) continue ;; esac

  match="$(jq -r --arg id "$id" '
    [( .vulnerabilities.list[]? | select(.advisory.id==$id) ),
     ( .warnings | to_entries[]? | .value[]? | select(.advisory!=null and .advisory.id==$id) )
    ][0] | if . == null then "" else "\(.package.name)\t\(.package.version)" end
  ' "$report")"

  if [ -z "$match" ]; then
    echo "DEAD  $id  - cargo audit, run with no ignore list, no longer matches this against the locked crate. Fixed upstream, or not a real dependency. Remove the ignore."
    dead=1
    continue
  fi

  name="${match%%$'\t'*}"; version="${match#*$'\t'}"
  err="$(mktemp)"
  if out="$(cargo tree -i "$name@$version" --workspace --target "$target" --edges all --all-features 2>"$err")"; then
    if [ -z "$out" ]; then
      echo "DEAD  $id  $name $version  - not reachable from the workspace on $target over any edge or feature (lockfile only). Remove the ignore; audit-build-graph.sh already tolerates a lockfile-only match without one."
      dead=1
    else
      echo "OK    $id  $name $version  - reachable and still open; the ignore is still needed."
    fi
  else
    echo "COULD NOT CLASSIFY $id $name $version - cargo tree failed, treated as dead:"
    cat "$err"
    dead=1
  fi
  rm -f "$err"
done

if [ "$dead" -ne 0 ]; then
  echo "FAIL: at least one ignored advisory no longer applies (see DEAD lines above)."
  exit 1
fi
echo "OK: every checked ignore still matches an unfixed advisory in a crate we actually build."
exit 0
