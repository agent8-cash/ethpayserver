#!/usr/bin/env bash
# Proves docs/backend-schema.md is the generator's output for the real
# migrations, and that the comparison goes red when either side drifts.
#
# Every comparison runs the generator for real and diffs its bytes against the
# checked-in file. Nothing here re-derives the schema a second way: a second
# derivation could agree with a wrong first one.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GEN="$ROOT/scripts/gen-backend-schema.py"
DOC="$ROOT/docs/backend-schema.md"
MIG="$ROOT/data-service/migrations/postgres"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
ok()  { echo "ok: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

# Exit 0 when running the generator on $1 reproduces the doc at $2 byte for byte.
matches() { # migrations_dir doc
  local out="$TMP/gen.$RANDOM.md"
  "$GEN" "$1" > "$out" 2>"$TMP/err" || { cat "$TMP/err" >&2; return 2; }
  cmp -s "$out" "$2"
}

# 1. The checked-in doc is what the generator produces from the real migrations.
matches "$MIG" "$DOC"; rc=$?
if [ "$rc" -eq 0 ]; then ok "checked-in doc matches generator output"
elif [ "$rc" -eq 2 ]; then bad "generator failed on the real migrations"
else bad "docs/backend-schema.md is stale: regenerate with scripts/gen-backend-schema.py > docs/backend-schema.md"; fi

# 2. Output is deterministic: a second run is byte-identical to the first.
"$GEN" "$MIG" > "$TMP/run1.md" && "$GEN" "$MIG" > "$TMP/run2.md" \
  && cmp -s "$TMP/run1.md" "$TMP/run2.md" \
  && ok "two runs are byte-identical" || bad "generator output differs between runs"

# 3. Negative control: a drifted doc must be caught by the same comparison.
cp "$DOC" "$TMP/drifted.md" && printf 'stray line\n' >> "$TMP/drifted.md"
if matches "$MIG" "$TMP/drifted.md"; then bad "a drifted doc still matched: the check cannot fail"
else ok "a drifted doc is refused"; fi

# 4. Negative control: a new migration must change the output. Proves the
#    generator reads the migrations directory rather than echoing the doc.
mkdir "$TMP/mig" && cp "$MIG"/*.sql "$TMP/mig/"
printf 'ALTER TABLE invoices ADD COLUMN zz_probe_column TEXT;\n' \
  > "$TMP/mig/99999999999999_probe.sql"
if matches "$TMP/mig" "$DOC"; then bad "an added migration did not change the output"
else ok "an added migration changes the output"; fi

[ "$fail" -eq 0 ] && echo "gen-backend-schema.test.sh: all checks passed"
exit "$fail"
