#!/usr/bin/env bash
# Proves secret-scan.sh goes red on a planted secret and green on a clean
# commit. The planted token is assembled at run time, so this file carries no
# literal credential. Exit status 1 is checked exactly: a missing scanner exits
# 127, and accepting "any non-zero" would let the red case pass with no scan.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCAN="$HERE/secret-scan.sh"
command -v gitleaks >/dev/null || { echo "FAIL: gitleaks not on PATH"; exit 2; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 2
git init -q && git config user.email t@example.invalid && git config user.name t
git config commit.gpgsign false
echo clean > a.txt && git add a.txt && git commit -qm base
base="$(git rev-parse HEAD)"

echo ordinary > b.txt && git add b.txt && git commit -qm clean
"$SCAN" "$base" >/dev/null 2>&1; rc=$?
if [ "$rc" -ne 0 ]; then echo "FAIL: clean commit exited $rc, expected 0"; exit 1; fi

token="ghp_$(head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n')"
echo "token=$token" > c.txt && git add c.txt && git commit -qm planted
"$SCAN" "$base" >/dev/null 2>&1; rc=$?
if [ "$rc" -ne 1 ]; then echo "FAIL: planted secret exited $rc, expected 1"; exit 1; fi

echo "secret-scan self-test: ok"
