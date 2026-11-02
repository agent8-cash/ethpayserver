#!/usr/bin/env bash
# Proves check-playwright-runner-pin.sh FIRES. A guard only ever observed to
# pass is indistinguishable from one that cannot fail - which is precisely the
# defect it was written for: ci.yml carried the pin while two other jobs
# floated, and nothing compared them.
set -uo pipefail
GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-playwright-runner-pin.sh"
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

# A job that runs playwright TESTS but installs no browser is not in scope -
# synthetic-payment does exactly this and drives the API, not a browser.
mkwf testsonly <<'EOS'
jobs:
  api:
    runs-on: ubuntu-latest
    steps:
      - run: npx playwright test tests/synthetic-payment.spec.ts
EOS
check "no browser install anywhere is COULD NOT RUN, not a pass" 2 "$TMP/testsonly"

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

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
