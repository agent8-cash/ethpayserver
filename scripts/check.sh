#!/usr/bin/env bash
# The definition of "done" for one unit of work. Exits non-zero on any failure.
#
# WHY THIS IS LONGER THAN fmt/clippy/test
# ---------------------------------------
# Those three were the obvious candidates and they are not sufficient here. On
# 2026-09-26 every defect that reached `testnet` was caught by something that is
# NOT one of them: a ticket id in public source, a migration filename, a file
# growing past the line limit, a commons pin that did not match the lock, and an
# end-to-end failure. A gate that runs three of nine checks and calls the result
# "done" is a green that means less than the one CI already gives us.
#
# So this runs every repository check that can run locally, plus the compiler
# ones, and it REPORTS WHAT IT DID NOT RUN rather than implying full coverage.
#
# WHAT THIS CANNOT TELL YOU
# -------------------------
# 656 of this workspace's 866 tests pass locally. The other 210 are `#[ignore]`d
# and need a live Postgres and Redis; CI runs them with --run-ignored and they
# gate merges the same as any other test. The end-to-end suite needs a real
# server and the pinned client image on top of that.
#
# So a green run here means "nothing local objects", not "this is mergeable".
# The difference is about a quarter of the gating tests, and it includes the
# tenant-isolation suite. Set DATABASE_URL and TEST_REDIS_URL to close most of
# the gap; the script says so at the end when they are unset.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)" || exit 1
export TMPDIR="${TMPDIR:-/var/tmp}"   # /tmp here is a RAM-backed tmpfs
export PATH="$HOME/.cargo/bin:$PATH"

failed=0
skipped=""

run() {
    local name="$1"; shift
    printf '  %-34s ' "$name"
    if out="$("$@" 2>&1)"; then
        echo "ok"
    else
        echo "FAIL"
        # HEAD of the failure, not the tail. Tailing showed the last 25 lines,
        # which for a test runner is the closing doctest summary rather than the
        # assertion that failed - so a session had to re-run the whole suite to
        # see what broke. The same truncation produced a wrong reading of the
        # baseline audit the day this script was written: cutting output is how
        # you end up answering "what did the last few lines say" instead of
        # "what failed".
        #
        # Both ends: the first lines carry the error, the last carry the summary.
        printf '%s\n' "$out" | sed 's/^/      /' | head -40
        if [ "$(printf '%s\n' "$out" | wc -l)" -gt 60 ]; then
            echo "      [...]"
            printf '%s\n' "$out" | sed 's/^/      /' | tail -20
        fi
        failed=$((failed + 1))
    fi
}

# First, so a slow run shows its job count before the slow part starts.
./scripts/cargo-jobs.sh
echo
echo "== repository checks =="
# Each of these exists because something got through once. Losing one of them
# from this list is how that thing gets through again.
#
# One exception, named rather than left to be discovered: the file-size check
# REPORTS and does not fail, as of 2026-09-26. It stays in this list because the
# report is now the only thing tracking a curve that doubled in nine days the
# last time nothing watched it. A green run here does not mean no file grew -
# read its line.
run "no ticket ids in source"   ./scripts/check-no-ticket-refs.sh
run "no session urls"           ./scripts/check-no-session-urls.sh
run "migration versions unique" ./scripts/check-migrations.sh
run "backend schema doc current" ./scripts/gen-backend-schema.test.sh
run "db tests never skip"       ./scripts/check-test-db-skips.sh
run "commons pin matches lock"  ./scripts/check-commons-pin.sh
run "commons crate detection"   ./scripts/commons.test.sh
run "file sizes (report only)"   ./scripts/check-file-size.sh

echo "== compiler =="
run "cargo fmt"                 cargo fmt --all -- --check
# --all-targets on purpose: the integration binaries under evm/tests and
# server/tests are compiled here. Five of them sat red for days once because
# nothing compiled them, while the gate reported green.
run "cargo clippy"              cargo clippy --workspace --all-targets -- -D warnings

echo "== tests =="
run "cargo test (workspace)"    cargo test --workspace --no-fail-fast

# Both, not just DATABASE_URL. Some of these tests need Redis as well, so with a
# database and no Redis they are skipped while this block reports as having run -
# a green that covers less than it says, which is the exact fault this script was
# written to avoid. Caught by a reader before it could mislead anyone.
if [ -n "${DATABASE_URL:-}" ] && [ -n "${TEST_REDIS_URL:-}" ]; then
    echo "== integration (DATABASE_URL and TEST_REDIS_URL set) =="
    # -j 1 is not cosmetic: these share one real Postgres and run in parallel
    # they produce failures CI never sees.
    run "integration tests"     cargo test --workspace --no-fail-fast -- --ignored --test-threads=1
else
    missing=""
    [ -z "${DATABASE_URL:-}" ] && missing="DATABASE_URL"
    [ -z "${TEST_REDIS_URL:-}" ] && missing="${missing:+$missing and }TEST_REDIS_URL"
    skipped="${skipped}  - integration tests: $missing unset. ~210 ignored tests did not run,
    including the tenant-isolation suite. CI runs these and they gate merges.
"
fi

skipped="${skipped}  - end-to-end: needs a running server and the pinned client image.
    See e2e/README.md. Rate limits must be raised or the suite fails for the
    wrong reason and logs nothing.
"

echo
if [ -n "$skipped" ]; then
    echo "NOT RUN BY THIS SCRIPT:"
    printf '%s' "$skipped"
    echo
fi

if [ "$failed" -ne 0 ]; then
    echo "FAILED: $failed check(s). This is not done."
    exit 1
fi

echo "All local checks passed. This is not the same as mergeable - see above."
exit 0
