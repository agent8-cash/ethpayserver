#!/usr/bin/env bash
# Proves check-runner-pin.sh FIRES. A guard only ever observed to
# pass is indistinguishable from one that cannot fail - which is precisely the
# defect it was written for: ci.yml carried the pin while two other jobs
# floated, and nothing compared them.
set -uo pipefail
GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-runner-pin.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
pass=0; fail=0

mkwf() { mkdir -p "$TMP/$1/.github/workflows"; cat > "$TMP/$1/.github/workflows/w.yml"; }

check() { # desc expected_rc root
  local desc="$1" want="$2" root="$3" out rc
  out=$(bash "$GUARD" "$root" 2>&1); rc=$?
  if [ "$rc" = "$want" ]; then
    echo "ok: $desc (rc $rc)"; pass=$((pass+1))
  else
    echo "FAIL: $desc - wanted rc $want, got $rc"; sed 's/^/    /' <<<"$out"; fail=$((fail+1))
  fi
}

mkwf pinned <<'EOS'
jobs:
  browser:
    runs-on: ubuntu-24.04
    steps:
      - run: npx playwright install --with-deps chromium
EOS
check "a pinned image passes" 0 "$TMP/pinned"

mkwf floating <<'EOS'
jobs:
  browser:
    runs-on: ubuntu-latest
    steps:
      - run: npx playwright install --with-deps chromium
EOS
check "ubuntu-latest FIRES - the real 2026-10-28 shape" 1 "$TMP/floating"

# The exact mixed state the repository was actually in for six days.
mkwf mixed <<'EOS'
jobs:
  e2e:
    runs-on: ubuntu-24.04
    steps:
      - run: npx playwright install --with-deps chromium
  browser:
    runs-on: ubuntu-latest
    steps:
      - run: npx playwright install --with-deps chromium
EOS
check "one job pinned and one floating FIRES, not 'mostly fine'" 1 "$TMP/mixed"

mkwf norunson <<'EOS'
jobs:
  browser:
    steps:
      - run: npx playwright install --with-deps chromium
EOS
check "a job with no runs-on at all FIRES" 1 "$TMP/norunson"

# SCOPE: every job, browser or not. This is the case the first version of this
# guard got wrong - it only looked at jobs installing a browser, so a floating
# build job (which breaks `e2e` through glibc/OpenSSL, not through chromium)
# passed silently.
mkwf nobrowser <<'EOS'
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: cargo build --release
EOS
check "a job with NO browser and a floating image FIRES" 1 "$TMP/nobrowser"

mkwf nobrowserpinned <<'EOS'
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - run: cargo build --release
EOS
check "a job with no browser, pinned, passes" 0 "$TMP/nobrowserpinned"

# A reusable-workflow job declares no runner of its own - the called workflow
# owns that choice, and judging it here would report on a file not present.
mkwf reusable <<'EOS'
jobs:
  call:
    uses: ./.github/workflows/other.yml
EOS
check "a reusable-workflow job is skipped, and skipping every job is COULD NOT RUN" 2 "$TMP/reusable"

# windows/macos float identically.
mkwf winfloat <<'EOS'
jobs:
  build:
    runs-on: windows-latest
    steps:
      - run: echo hi
EOS
check "windows-latest FIRES - any *-latest spelling" 1 "$TMP/winfloat"

mkdir -p "$TMP/noworkflows"
check "a tree with no workflows directory is COULD NOT RUN" 2 "$TMP/noworkflows"

mkwf unparseable <<'EOS'
jobs:
  browser:
    runs-on: [ubuntu-24.04
EOS
check "an unparseable workflow is COULD NOT RUN, never skipped" 2 "$TMP/unparseable"

# macos/windows float the same way and must be refused for the same reason.
mkwf macfloat <<'EOS'
jobs:
  browser:
    runs-on: macos-latest
    steps:
      - run: npx playwright install --with-deps webkit
EOS
check "macos-latest FIRES too - the defect is the float, not the OS" 1 "$TMP/macfloat"

# A newer pinned image passes: the contract is that somebody chose, not which.
mkwf pinnednewer <<'EOS'
jobs:
  browser:
    runs-on: ubuntu-26.04
    steps:
      - run: npx playwright install --with-deps chromium
EOS
check "a pinned 26.04 passes - the guard pins the choice, not the version" 0 "$TMP/pinnednewer"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
