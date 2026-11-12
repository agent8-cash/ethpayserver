#!/usr/bin/env bash
# Proves check-sensitive-keys-agree.sh goes red and green on the cases it claims
# to guard. Builds a synthetic commons (a shared SENSITIVE_KEYS list) and a
# synthetic consumer pinned to it, then runs the real guard against each state.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guard_src="$repo_root/scripts/check-sensitive-keys-agree.sh"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

commons="$workdir/commons"
mkdir -p "$commons/scrub/src"
cat > "$commons/scrub/src/lib.rs" <<'EOF'
pub const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "auth",
    "dsn",
    "bearer",
];
EOF
git -C "$commons" init -q -b main
git -C "$commons" add -A
git -C "$commons" commit -q -m shared
rev="$(git -C "$commons" rev-parse HEAD)"

consumer="$workdir/consumer"
mkdir -p "$consumer/scripts" "$consumer/evm/src"
cp "$guard_src" "$consumer/scripts/check-sensitive-keys-agree.sh"
cat > "$consumer/Cargo.toml" <<EOF
[workspace.dependencies]
scrub = { git = "https://example.invalid/payserver-commons.git", rev = "$rev" }
EOF
cat > "$consumer/evm/src/telemetry.rs" <<'EOF'
use scrub::is_sensitive_key;

pub fn redact(key: &str) -> bool {
    is_sensitive_key(key)
}
EOF
cat > "$consumer/evm/src/lib_tests.rs" <<'EOF'
#[test]
fn every_key() {
    for key in scrub::SENSITIVE_KEYS.iter().copied() {
        assert!(scrub::is_sensitive_key(key));
    }
}
EOF
git -C "$consumer" init -q -b main

stage() { git -C "$consumer" add -A; }
run_guard() {
  stage
  COMMONS_URL="$commons" "$consumer/scripts/check-sensitive-keys-agree.sh" >"$workdir/out" 2>&1
}

fail=0
expect() { # expect <green|red> <label>
  local want="$1" label="$2" got
  if run_guard; then got=green; else got=red; fi
  if [ "$got" = "$want" ]; then
    echo "ok   $label: $got"
  else
    echo "FAIL $label: want $want, got $got"; cat "$workdir/out"; fail=1
  fi
}

expect green "consumer uses the shared list and restates nothing"

# Ablation 1: a consumer defines its own predicate.
cp "$consumer/evm/src/telemetry.rs" "$workdir/t.bak"
cat >> "$consumer/evm/src/telemetry.rs" <<'EOF'
fn is_sensitive_key(key: &str) -> bool { key.contains("x") }
EOF
expect red "local is_sensitive_key definition"
cp "$workdir/t.bak" "$consumer/evm/src/telemetry.rs"

# Ablation 2: a consumer restates the list in an array.
cat > "$consumer/evm/src/restated.rs" <<'EOF'
pub fn keys() {
    for key in [
        "password",
        "auth",
    ] {
        let _ = key;
    }
}
EOF
expect red "restated key array"

# Ablation 2b: a typed const literal restates the list (the shape billing used).
cat > "$consumer/evm/src/restated.rs" <<'EOF'
pub fn keys() {
    const KEYS: [&str; 2] = [
        "password",
        "auth",
    ];
    let _ = KEYS;
}
EOF
expect red "typed const array restating the list"
rm "$consumer/evm/src/restated.rs"

# Ablation 3: a restated array gains a key the shared list lacks.
cat > "$consumer/evm/src/restated.rs" <<'EOF'
pub fn keys() {
    let keys = &["password", "auth", "pin"];
    let _ = keys;
}
EOF
expect red "array with a key the shared list lacks"
rm "$consumer/evm/src/restated.rs"

expect green "restored"

if [ "$fail" -ne 0 ]; then echo "check-sensitive-keys-agree.test.sh: FAILED"; exit 1; fi
echo "check-sensitive-keys-agree.test.sh: ok"
