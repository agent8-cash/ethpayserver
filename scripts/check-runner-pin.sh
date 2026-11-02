#!/usr/bin/env bash
# EVERY workflow job must name its runner image. Not only the browser jobs.
#
# `ubuntu-latest` is chosen by GitHub, not by us. It moved to Ubuntu 26.04 and
# @playwright/test 1.59.1 publishes no chromium build for 26.04, so from
# 2026-10-28 the scheduled browser job died in `playwright install` -
#
#   ERROR: Playwright does not support chromium on ubuntu26.04-x64
#
# - before one test ran, every day, for six days. The daily E2E reported
# failure the whole time and the failure was environmental, so the signal
# people actually wanted (does a payment still work end to end) was neither
# green nor red, it was absent.
#
# ci.yml's `e2e` job was ALREADY pinned to ubuntu-24.04 against this same
# drift. The repository therefore held the fix and the bug at once, in two
# files, for six days - which is the thing this guard exists to make
# impossible. A fix applied to one of three jobs is not a fix, and nothing
# compared them.
#
# SCOPE IS EVERY JOB, and that is the owner's standing instruction: lock the
# Ubuntu version, do not run `-latest`. An earlier pass pinned only the jobs
# that install a browser, on the reasoning that those were the ones observed
# to break. That reasoning is wrong twice over. It treats the symptom that
# happened to surface as the boundary of the defect, and it had already failed
# once here: `build` carries a comment explaining that it must be pinned
# because `e2e` runs its binary on the runner's own glibc and OpenSSL, so a
# 26.04-built binary refuses to start on 24.04 with
#
#   libcrypto.so.3: version 'OPENSSL_3.2.0' not found
#
# which read as a server crash rather than a runner mismatch. A job does not
# have to install a browser to be broken by the image moving under it.
#
# Pinned means an explicit image. Any `*-latest` spelling is refused;
# `ubuntu-24.04`, `ubuntu-26.04` or a self-hosted label all pass, because the
# point is that a human chose it, not which one they chose. Moving the fleet
# to a newer image is then a visible one-line diff per job rather than
# something that happens to us on GitHub's schedule.
#
#   rc 0  every job names its image
#   rc 1  at least one floats
#   rc 2  COULD NOT RUN - no workflows, no jobs, or a workflow that will not
#         parse. Never a pass: a guard that reports clean over a set it could
#         not read is the failure mode this whole family exists to avoid.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${1:-$(cd "$HERE/.." && pwd)}"
WFDIR="$ROOT/.github/workflows"
# Kept only to label the browser jobs in the output - they are the ones whose
# float cost six days of absent E2E signal - not to select what is checked.
MARKER="${PLAYWRIGHT_INSTALL_MARKER:-playwright install}"

[ -d "$WFDIR" ] || { echo "FAILED: no $WFDIR - refusing to report clean over an empty set"; exit 2; }

found=0
fail=0
unreadable=0
# Parsed with python, not grep: `runs-on` and the install step sit at
# different depths in different files, and a line-based reading cannot tell
# which job a `runs-on` belongs to. Getting that wrong is how a guard reports
# on a job other than the one it names.
out=$(python3 - "$WFDIR" "$MARKER" <<'PY'
import sys, pathlib, yaml
wfdir, marker = pathlib.Path(sys.argv[1]), sys.argv[2]
rows = []
for f in sorted(wfdir.glob("*.yml")) + sorted(wfdir.glob("*.yaml")):
    try:
        doc = yaml.safe_load(f.read_text())
    except Exception as e:                      # a workflow we cannot parse is not a pass
        # Flattened to one line: a yaml error is multi-line, and a reader
        # splitting on newlines would read the remainder as further jobs.
        reason = " ".join(str(e).split())
        print(f"UNPARSEABLE\t{f.name}\t-\t{reason}")
        continue
    if not isinstance(doc, dict):
        continue
    for name, job in (doc.get("jobs") or {}).items():
        if not isinstance(job, dict):
            continue
        note = "browser" if marker in yaml.dump(job) else "-"
        # A reusable-workflow job has no runs-on of its own; the called
        # workflow owns that choice and this file cannot judge it.
        if "uses" in job and "runs-on" not in job:
            note = "reusable"
        rows.append((f.name, name, job.get("runs-on"), note))
for f, n, r, note in rows:
    print(f"JOB\t{f}\t{n}\t{r}\t{note}")
PY
) || { echo "FAILED: could not read the workflow files - the check COULD NOT RUN (not a pass)"; exit 2; }

while IFS=$'\t' read -r kind file job runs_on note; do
    [ -n "${kind:-}" ] || continue
    if [ "$kind" = "UNPARSEABLE" ]; then
        # COULD NOT RUN, not FAIL. A workflow this cannot read might contain a
        # floating job or might not; reporting it as "a job floats" states
        # something unmeasured, and reporting it as clean is worse.
        echo "COULD NOT PARSE $file - the check COULD NOT RUN over it: $runs_on"
        unreadable=1
        continue
    fi
    if [ "${note:-}" = "reusable" ]; then
        echo "skip    $file job '$job' calls a reusable workflow, which owns its own runner"
        continue
    fi
    found=$((found + 1))
    label=""
    [ "${note:-}" = "browser" ] && label=" (installs a browser)"
    case "$runs_on" in
        *-latest*)
            echo "FAILED  $file job '$job'$label runs on '$runs_on' - a floating image decides whether this job can run at all, and GitHub picks when it moves"
            fail=1
            ;;
        None|"")
            echo "FAILED  $file job '$job'$label declares no runs-on"
            fail=1
            ;;
        *)
            echo "ok      $file job '$job'$label -> $runs_on"
            ;;
    esac
done <<EOF
$out
EOF

if [ "$unreadable" = 1 ]; then
    echo "COULD NOT RUN: at least one workflow could not be parsed, so this is not a verdict about the jobs in it."
    exit 2
fi

if [ "$found" = 0 ]; then
    echo "FAILED: no job with its own runs-on was found in $WFDIR - reporting clean here would be reporting on nothing."
    exit 2
fi

[ "$fail" = 0 ] || { echo "FAIL: $found job(s) checked, at least one floats."; exit 1; }
echo "OK: all $found job(s) name their runner image."
