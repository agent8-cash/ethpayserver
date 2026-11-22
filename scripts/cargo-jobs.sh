#!/usr/bin/env bash
# Prints the cargo job count this run will use and where it came from, e.g.
#   cargo jobs: 1 (<dir>/.cargo/config.toml)
# Mirrors cargo's precedence: CARGO_BUILD_JOBS, then the nearest [build] jobs
# walking up from the start directory, then CARGO_HOME, then the core count.
# A slow build on a capped host should read as slow, not as hung.
#
# Usage: cargo-jobs.sh [START_DIR]   (default: the git top-level)
set -uo pipefail

report() { echo "cargo jobs: $1 ($2)"; exit 0; }

if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
  report "$CARGO_BUILD_JOBS" CARGO_BUILD_JOBS
fi

# jobs_in FILE: the integer value of `jobs` under [build] in FILE, if any.
jobs_in() {
  awk '
    /^[[:space:]]*\[/ { in_build = ($0 ~ /^[[:space:]]*\[build\][[:space:]]*(#.*)?$/); next }
    in_build && /^[[:space:]]*jobs[[:space:]]*=/ {
      v = $0
      sub(/^[^=]*=[[:space:]]*/, "", v)
      sub(/[[:space:]]*(#.*)?$/, "", v)
      if (v ~ /^[0-9]+$/) { print v; exit }
    }' "$1"
}

dir="$(cd "${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}" && pwd)"
while :; do
  for f in "$dir/.cargo/config.toml" "$dir/.cargo/config"; do
    if [ -f "$f" ] && v="$(jobs_in "$f")" && [ -n "$v" ]; then
      report "$v" "$f"
    fi
  done
  [ "$dir" = / ] && break
  dir="$(dirname "$dir")"
done

home="${CARGO_HOME:-$HOME/.cargo}"
for f in "$home/config.toml" "$home/config"; do
  if [ -f "$f" ] && v="$(jobs_in "$f")" && [ -n "$v" ]; then
    report "$v" "$f"
  fi
done

report "$(nproc)" "default: one per core"
