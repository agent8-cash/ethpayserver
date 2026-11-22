#!/usr/bin/env bash
# Proves check-lockfile-current.sh goes red and green on the cases it claims
# to guard, instead of asserting that in a comment. A guard whose red/green
# behaviour was only ever described by its own author is exactly the shape this
# ticket exists to prevent.
#
# Creates synthetic Cargo.toml and Cargo.lock files in a fixture directory,
# with deliberate mismatches to assert the guard detects them in both directions.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guard="$repo_root/scripts/check-lockfile-current.sh"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

fixture="$workdir/fixture"
mkdir -p "$fixture"
cp "$guard" "$fixture/check-lockfile-current.sh"

pass=0
fail=0

run_guard() {
  MANIFEST_DIR="$fixture" "$fixture/check-lockfile-current.sh" \
    >"$workdir/out" 2>&1
}

assert_exit() {
  local desc="$1" want="$2"
  shift 2
  local rc=0
  run_guard || rc=$?
  if [ "$rc" -eq "$want" ]; then
    echo "ok: $desc (exit $rc)"
    pass=$((pass + 1))
  else
    echo "FAIL: $desc - wanted exit $want, got $rc"
    sed 's/^/  /' "$workdir/out"
    fail=$((fail + 1))
  fi
}

# Create a minimal Cargo.toml with a dependency, then generate its lock file
mkdir -p "$fixture/src"
cat > "$fixture/Cargo.toml" <<'EOF'
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
EOF

# Create a minimal lib.rs so cargo is happy
cat > "$fixture/src/lib.rs" <<'EOF'
pub fn lib() {}
EOF

# Generate a valid lock file by running cargo update
if ! cargo update --manifest-path "$fixture/Cargo.toml" -q 2>/dev/null; then
  echo "SKIP: could not generate lock file for fixture"
  exit 0
fi

# Verify the valid case: fixture manifest and lock are in sync (green)
assert_exit "fixture manifest and lock are in sync (green)" 0

# Now test the red case: modify Cargo.toml to introduce a version that doesn't
# exist in the lock file.
cat > "$fixture/Cargo.toml" <<'EOF'
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "999.0"
EOF

# Keep the old lock (which is now stale relative to Cargo.toml).
# cargo metadata --locked should fail because the lock doesn't have serde 999.0
assert_exit "manifest version bumped, lock not refreshed (red)" 1

# Verify the red case's output contains "Cargo.lock" as the review comment requires
if grep -q "Cargo.lock" "$workdir/out"; then
  echo "ok: error message names Cargo.lock"
  pass=$((pass + 1))
else
  echo "FAIL: error message should name Cargo.lock"
  sed 's/^/  /' "$workdir/out"
  fail=$((fail + 1))
fi

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
