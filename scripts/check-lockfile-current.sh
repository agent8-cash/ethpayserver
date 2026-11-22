#!/usr/bin/env bash
# Refuse a Cargo.lock that does not satisfy the pinned Cargo.toml.
#
# A Cargo.toml dependency bump without a corresponding Cargo.lock update
# silently skips security checks (cargo audit, cargo deny) that run in CI.
# The commit passes local fmt, clippy, and test checks without running
# cargo audit or cargo deny, hiding vulnerabilities that would be caught
# in CI.
#
# Example defect: mcp-server/Cargo.toml bumps rmcp from "2.0" to "3",
# but the lock file is left at rmcp 2.1.0. Local checks pass; CI audit fails.
# This guard catches that mismatch before pushing.
#
# Uses `cargo metadata --locked` which fails when Cargo.lock does not satisfy
# all Cargo.toml pinned versions. With --locked, cargo refuses to update the
# lock file and fails if the current lock is inconsistent.
#
# MANIFEST_DIR override (test only): when set, specifies the directory containing
# Cargo.toml and Cargo.lock, instead of reading from repo root. The fixture test
# uses this to run the guard against a synthetic manifest without touching the
# real repo's lock file.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Overridable for test fixtures.
manifest_dir="${MANIFEST_DIR:-$repo_root}"

# cargo metadata --locked fails with exit 1 when the lock file is
# inconsistent with Cargo.toml. We use --format-version 1 to get stable
# output, and discard stdout; we only care about whether it succeeds.
if ! cargo metadata --locked --format-version 1 \
    --manifest-path "$manifest_dir/Cargo.toml" >/dev/null 2>&1; then
  echo "::error::Cargo.lock is out of date or inconsistent with Cargo.toml"
  echo "  Run 'cargo update' to reconcile, then commit the updated Cargo.lock"
  echo "  A Cargo.toml-only change that skips updating Cargo.lock hides"
  echo "  security vulnerabilities that would be caught by cargo audit and cargo deny."
  exit 1
fi

echo "ok: Cargo.lock is consistent with Cargo.toml"
exit 0
