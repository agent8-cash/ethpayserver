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
# A FIXTURE MAY SUBSTITUTE THE REPORT ONLY WHEN THE CALLER SAYS SO EXPLICITLY.
#
# AUDIT_JSON_FILE exists so this script's own test can feed it a crafted report.
# Read from the environment alone, it is also a silent bypass: anything that sets
# it - a shell profile, a CI env block, a persistent runner's .env - replaces the
# real advisory scan with a file, and the audit then reports a pass having scanned
# nothing. The default is right and the default is not the control.
#
# Not reachable today, because this job runs on a GitHub-hosted runner: a fresh VM
# per job, no .env carried between runs. But that is a CONFIGURATION, not a
# property of this script, and moving the job to a self-hosted runner would arm
# the bypass invisibly. The marker makes the safety belong to the script, so where
# it runs becomes a cost question rather than a security one.
#
# Two variables rather than one is deliberate. AUDIT_JSON_FILE names a file, which
# has no narrower-or-wider sense, so it cannot be made safe by requiring an
# override to narrow: an unordered substitution needs an explicit marker instead.
# Fails with exit 2 - could not run - because an audit that was handed a fixture
# it was not told to expect has not audited anything, and that is not a verdict
# about the code.
if [ -n "${AUDIT_JSON_FILE:-}" ]; then
  if [ "${AUDIT_FIXTURE_OK:-}" != 1 ]; then
    echo "ERROR: AUDIT_JSON_FILE is set but AUDIT_FIXTURE_OK is not 1 - refusing to substitute a fixture for the real scan. The audit COULD NOT RUN (not a pass)." >&2
    exit 2
  fi
  echo "NOTE: using the fixture at $AUDIT_JSON_FILE instead of cargo audit (AUDIT_FIXTURE_OK=1)" >&2
  cp "$AUDIT_JSON_FILE" "$report"
else
  cargo audit --json > "$report"
fi

if ! jq -e '.vulnerabilities.list | type == "array" and all(.[]; type == "object")' "$report" >/dev/null 2>&1; then
  echo "ERROR: cargo audit produced no parseable report - the audit COULD NOT RUN (not a pass)."
  head -c 2000 "$report"
  exit 2
fi

# Read the entries into a variable first: a jq failure inside `< <(...)` is
# invisible to the loop and would read as "no advisories".
entries=$(jq -r '.vulnerabilities.list[] | [.advisory.id // "", .package.name // "", .package.version // ""] | @tsv' "$report") || {
  echo "ERROR: could not read the advisory entries - the audit COULD NOT RUN (not a pass)."
  exit 2
}

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
      echo "::warning::LOCKFILE ONLY  $id  $name $version - not reachable from the workspace on $target; does not block"
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
done < <(printf '%s\n' "$entries" | sed '/^$/d')

if [ "$blocking" -ne 0 ]; then
  echo "FAIL: at least one advisory is in a crate we build (or could not be classified)."
  exit 1
fi
echo "OK: no advisory in a crate we build."
