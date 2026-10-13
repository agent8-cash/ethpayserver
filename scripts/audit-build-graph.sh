#!/usr/bin/env bash
# `cargo audit` reads Cargo.lock, which records every platform's and every
# optional feature's dependencies whether or not anything compiles them. An
# advisory in a crate that only the lockfile mentions would block merges for
# code we never build. This runs cargo audit and then asks cargo itself whether
# each flagged crate is reachable from any workspace member on the target the
# containers are built for (BUILD_TARGET, default x86_64 Linux), over every edge
# kind (normal, build, dev) and every feature. Deliberately NOT `--target all`:
# that answers "is it in the lockfile", which is what cargo audit already says.
#
#   in the build graph  -> BLOCKING, exit 1
#   lockfile only       -> reported loudly, does not fail
#
# Anything unclear fails closed: audit output we cannot parse, an advisory entry
# we cannot read, or a cargo tree error for a flagged crate is never reported as
# "lockfile only".
set -uo pipefail

report=$(mktemp); trap 'rm -f "$report"' EXIT
if [ -n "${AUDIT_JSON_FILE:-}" ]; then cp "$AUDIT_JSON_FILE" "$report"; else cargo audit --json > "$report"; fi

if ! jq -e '.vulnerabilities.list' "$report" >/dev/null 2>&1; then
  echo "ERROR: cargo audit produced no parseable report - the audit COULD NOT RUN (not a pass)."
  head -c 2000 "$report"
  exit 2
fi

target="${BUILD_TARGET:-x86_64-unknown-linux-gnu}"
blocking=0
while IFS=$'\t' read -r id name version; do
  if [ -z "$id" ] || [ -z "$name" ] || [ -z "$version" ]; then
    echo "COULD NOT CLASSIFY an advisory entry (id='$id' name='$name' version='$version') - BLOCKING"
    blocking=1
    continue
  fi
  err=$(mktemp)
  if out=$(cargo tree -i "$name@$version" --workspace --target "$target" --edges all --all-features 2>"$err"); then
    if [ -z "$out" ]; then
      echo "LOCKFILE ONLY  $id  $name $version - not reachable from the workspace on $target; does not block"
      rm -f "$err"
      continue
    fi
    echo "IN BUILD GRAPH $id  $name $version - BLOCKING"
  else
    echo "COULD NOT CLASSIFY $id  $name $version - cargo tree failed, treated as BLOCKING:"
    cat "$err"
  fi
  rm -f "$err"
  blocking=1
done < <(jq -r '.vulnerabilities.list[] | [.advisory.id // "", .package.name // "", .package.version // ""] | @tsv' "$report")

if [ "$blocking" -ne 0 ]; then
  echo "FAIL: at least one advisory is in a crate we build (or could not be classified)."
  exit 1
fi
echo "OK: no advisory in a crate we build."
