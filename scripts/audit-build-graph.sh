#!/usr/bin/env bash
# `cargo audit` reads Cargo.lock, which records every platform's and every
# optional feature's dependencies whether or not anything compiles them. An
# advisory in a crate that only the lockfile mentions would block merges for
# code we never build. This runs cargo audit and then asks cargo itself whether
# each flagged crate is reachable from the workspace on any target, any edge
# kind, any feature:
#
#   in the build graph  -> BLOCKING, exit 1
#   lockfile only       -> reported loudly, does not fail
#
# Anything unclear fails closed: audit output we cannot parse, or a cargo tree
# error for a flagged crate, is treated as "in the build graph".
set -uo pipefail

report=$(mktemp); trap 'rm -f "$report"' EXIT
if [ -n "${AUDIT_JSON_FILE:-}" ]; then cp "$AUDIT_JSON_FILE" "$report"; else cargo audit --json > "$report"; fi

if ! jq -e '.vulnerabilities.list' "$report" >/dev/null 2>&1; then
  echo "ERROR: cargo audit produced no parseable report - the audit COULD NOT RUN (not a pass)."
  head -c 2000 "$report"
  exit 2
fi

blocking=0
while IFS=$'\t' read -r id name version; do
  [ -n "$id" ] || continue
  if out=$(cargo tree -i "$name@$version" --target all --edges all --all-features 2>/dev/null); then
    if [ -z "$out" ]; then
      echo "LOCKFILE ONLY  $id  $name $version - not reachable from the workspace on any target; does not block"
      continue
    fi
  fi
  echo "IN BUILD GRAPH $id  $name $version - BLOCKING"
  blocking=1
done < <(jq -r '.vulnerabilities.list[] | [.advisory.id, .package.name, .package.version] | @tsv' "$report")

if [ "$blocking" -ne 0 ]; then
  echo "FAIL: at least one advisory is in a crate we build."
  exit 1
fi
echo "OK: no advisory in a crate we build."
