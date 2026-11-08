#!/usr/bin/env bash
# Proves cargo-jobs.sh reports the job count cargo will actually use, and where
# it came from, on each precedence rung. Every case gets its own repo and its
# own CARGO_HOME so the developer's real ~/.cargo config cannot leak in.
set -uo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/cargo-jobs.sh"
TMP="$(TMPDIR=/var/tmp mktemp -d)"   # /tmp is RAM-backed here
trap 'rm -rf "$TMP"' EXIT

fail=0
n=0

# new_case: fresh repo dir and CARGO_HOME for one case.
new_case() {
  n=$((n + 1))
  CASE="$TMP/case$n"
  REPO="$CASE/repo"
  CH="$CASE/cargo-home"
  mkdir -p "$REPO" "$CH"
}

# jobs_line [VAR=value ...]: run the script in REPO with a clean environment.
jobs_line() {
  env -u CARGO_BUILD_JOBS CARGO_HOME="$CH" "$@" "$SCRIPT" "$REPO" 2>&1
}

# expect NAME WANT_SUBSTRING [VAR=value ...]
expect() {
  local name="$1" want="$2" got
  shift 2
  got="$(jobs_line "$@")"
  if [ "$got" = "$want" ]; then
    echo "ok: $name"
  else
    echo "FAIL: $name - want '$want', got '$got'"
    fail=1
  fi
}

write_repo_config() { # write_repo_config DIR CONTENT
  mkdir -p "$1/.cargo"
  printf '%b' "$2" > "$1/.cargo/config.toml"
}

ncpu="$(nproc)"

new_case
write_repo_config "$REPO" '[build]\njobs = 2\n'
expect "env var beats every config file" \
  "cargo jobs: 3 (CARGO_BUILD_JOBS)" CARGO_BUILD_JOBS=3

new_case
write_repo_config "$REPO" '[build]\njobs = 2\n'
expect "repo .cargo/config.toml names its file" \
  "cargo jobs: 2 ($REPO/.cargo/config.toml)"

new_case
printf '[build]\njobs = 1\n' > "$CH/config.toml"
expect "CARGO_HOME config is used when the repo sets nothing" \
  "cargo jobs: 1 ($CH/config.toml)"

new_case
write_repo_config "$REPO" '[build]\njobs = 2\n'
printf '[build]\njobs = 1\n' > "$CH/config.toml"
expect "nearest config wins over CARGO_HOME" \
  "cargo jobs: 2 ($REPO/.cargo/config.toml)"

new_case
mkdir -p "$CASE/.cargo" "$CASE/deeper/repo"
printf '[build]\njobs = 4\n' > "$CASE/.cargo/config.toml"
REPO="$CASE/deeper/repo"
expect "config in an ancestor directory is found" \
  "cargo jobs: 4 ($CASE/.cargo/config.toml)"

new_case
write_repo_config "$REPO" '# [build]\n# jobs = 4\n[net]\njobs = 5\n'
expect "commented-out and other-section jobs are ignored" \
  "cargo jobs: $ncpu (default: one per core)"

new_case
write_repo_config "$REPO" '[build]\njobs = 2 # two cores\n'
expect "trailing comment after the value is stripped" \
  "cargo jobs: 2 ($REPO/.cargo/config.toml)"

new_case
expect "no config anywhere reports the core-count default" \
  "cargo jobs: $ncpu (default: one per core)"

[ "$fail" -eq 0 ] && echo "cargo-jobs.sh behaves as documented"
exit "$fail"
