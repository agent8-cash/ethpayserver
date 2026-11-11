#!/usr/bin/env bash
# Proves commons.sh link finds all the crates and generates a working patch,
# instead of silently patching nothing when the URL is outdated. The script has
# a hardcoded commons URL; if it gets out of sync with the manifest, link reports
# success while the patch matches no crates and nothing gets locally tested.
#
# Builds a synthetic workspace with crates matching the real Cargo.toml and
# verifies the patch config names every one of them.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
commons_sh="$repo_root/scripts/commons.sh"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# Synthetic commons checkout with subdirectories for each crate.
commons="$workdir/commons"
for crate in api-types auth crypto payserver-plugin-api payserver-plugin-host rates scrub types; do
  mkdir -p "$commons/$crate"
done

# Synthetic workspace with a manifest that matches the real Cargo.toml's crates.
workspace="$workdir/workspace"
mkdir -p "$workspace/scripts"
workspace_manifest="$workspace/Cargo.toml"
workspace_config="$workspace/.cargo/config.toml"

cat > "$workspace_manifest" <<'EOF'
[workspace]
members = []

[workspace.dependencies]
api-types = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
auth = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
crypto = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
payserver-plugin-api = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
payserver-plugin-host = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
rates = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
scrub = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
types = { git = "https://github.com/agent8-cash/payserver-commons.git", rev = "abc123" }
EOF

# Initialize a git repo so commons.sh can find the root
cd "$workspace"
git init -q
git config user.email "test@test"
git config user.name "Test"

# Copy the script to scripts/ so the relative path resolution works
cp "$commons_sh" "$workspace/scripts/commons.sh"
chmod +x "$workspace/scripts/commons.sh"

# Run link from the scripts directory
cd "$workspace/scripts"
./commons.sh link "$commons" > /tmp/link_output.txt 2>&1 || true

# Back to top of workspace to verify config was created
cd "$workspace"

if [ ! -f "$workspace_config" ]; then
  echo "FAIL: .cargo/config.toml not created"
  cat /tmp/link_output.txt 2>&1 || true
  exit 1
fi

# Verify all 8 crates are in the config
expected_crates=(
  api-types
  auth
  crypto
  payserver-plugin-api
  payserver-plugin-host
  rates
  scrub
  types
)

pass=0
fail=0

for crate in "${expected_crates[@]}"; do
  if grep -q "^$crate = { path" "$workspace_config"; then
    pass=$((pass + 1))
  else
    echo "FAIL: crate '$crate' not found in patch config"
    cat "$workspace_config"
    fail=$((fail + 1))
  fi
done

echo "linked config: $pass crates found, $fail missing"

# Verify the patch block is present
if ! grep -q '\[patch\."https://github.com/agent8-cash/payserver-commons.git"\]' "$workspace_config"; then
  echo "FAIL: patch URL not correct in config"
  cat "$workspace_config"
  exit 1
fi

# Test that unlink removes the config
cd "$workspace/scripts"
./commons.sh unlink > /tmp/unlink_output.txt 2>&1 || true
cd "$workspace"

if [ -f "$workspace_config" ]; then
  echo "FAIL: config not removed by unlink"
  exit 1
fi

exit "$fail"
