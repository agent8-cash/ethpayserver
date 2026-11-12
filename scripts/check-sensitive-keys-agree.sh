#!/usr/bin/env bash
# Refuse a consumer that keeps its own sensitive-key list.
#
# The redaction key set lives once, in payserver-commons `scrub`. A consumer
# that restates it in a literal array, or defines its own `is_sensitive_key`,
# is a second list that nothing compares: the three lists this guards against
# disagreed for months, and each service redacted a different set of keys on
# its own error paths. This check reads the shared list at the revision this
# repository pins, and fails when a Rust source file under the redaction seam
#
#   - defines its own `is_sensitive_key` or `SENSITIVE` / `SENSITIVE_KEYS`, or
#   - carries a literal array holding two or more of the shared key names.
#
# A consumer that needs a key the shared list lacks cannot add it locally: the
# array it would add the key to already trips the second rule. The fix is to
# extend the shared list, visibly, in commons.
#
# Like check-commons-pin.sh, the shared list is read from a clone of commons, not
# a sibling checkout, so the check means the same thing in CI and on a laptop.
# Must-fail-first is shown by check-sensitive-keys-agree.test.sh, which builds a
# synthetic commons and consumer and asserts this script goes red on each case.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Overridable so the test can point at a synthetic commons.
commons_override="${COMMONS_URL:-}"

pins="$(find . -name Cargo.toml -not -path '*/target/*' -print0 \
  | xargs -0 grep -hE '^scrub = \{ git = ' || true)"
if [ -z "$pins" ]; then
  echo "::error::no scrub pin found in any Cargo.toml"
  exit 1
fi

revs="$(printf '%s\n' "$pins" | sed -nE 's/.*rev = "([^"]*)".*/\1/p' | sort -u)"
if [ "$(printf '%s\n' "$revs" | wc -l)" -ne 1 ]; then
  echo "::error::scrub is pinned to more than one revision:"
  printf '%s\n' "$revs" | sed 's/^/  /'
  exit 1
fi
rev="$revs"
if ! [[ "$rev" =~ ^[0-9a-f]{40}$ ]]; then
  echo "::error::scrub rev is not a full 40-character sha: $rev"
  exit 1
fi

url="$(printf '%s\n' "$pins" | sed -nE 's/.*git = "([^"]*)".*/\1/p' | head -1)"
url="${commons_override:-$url}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if ! git clone --quiet --bare "$url" "$work/commons" 2>/dev/null; then
  echo "::error::could not clone payserver-commons from $url"
  exit 1
fi
if ! git -C "$work/commons" cat-file -e "$rev^{commit}" 2>/dev/null; then
  echo "::error::pinned commons rev $rev is not in $url"
  exit 1
fi

shared="$work/shared.txt"
git -C "$work/commons" show "$rev:scrub/src/lib.rs" \
  | awk '/pub const SENSITIVE_KEYS/ {f=1; next} f && /^\];/ {f=0} f' \
  | grep -oP '"\K[^"]+(?=")' > "$shared" || true
keys="$(wc -l < "$shared")"
# A list that reads as empty would pass every check below. Refuse that.
if [ "$keys" -lt 1 ]; then
  echo "::error::could not read SENSITIVE_KEYS from scrub/src/lib.rs at $rev"
  exit 1
fi

failed=0
bad() { echo "::error::$1"; failed=$((failed + 1)); }

# 1. No second definition of the predicate or the list.
defs="$(git grep -nE 'fn is_sensitive_key|const SENSITIVE(_KEYS)?\b' -- '*.rs' || true)"
if [ -n "$defs" ]; then
  while IFS= read -r line; do
    bad "a consumer defines its own sensitive-key list or predicate: $line"
  done <<< "$defs"
fi

# 2. Every file that redacts through the shared scrubber, and no literal array
# holding two or more shared key names beside it.
seam="$(git grep -l -E 'scrub::|redact_secrets|is_sensitive_key' -- '*.rs' || true)"
if [ -z "$seam" ]; then
  bad "no Rust file uses the shared scrubber, so there is nothing for this check to agree with"
fi
# The array rule covers every tracked Rust file, not only the seam: a second
# list does not need to sit beside the scrubber to be a second list.
all_rs="$(git ls-files -- '*.rs')"
if [ -n "$all_rs" ]; then
  restated="$(printf '%s\n' "$all_rs" | xargs awk -v sharedf="$shared" '
    BEGIN { while ((getline l < sharedf) > 0) S[l] = 1 }
    {
      if (!inarr && ($0 ~ /&\[/ || $0 ~ /\[&str/ || $0 ~ / in \[/)) { inarr = 1; start = FNR; k = 0 }
      if (inarr) {
        s = $0
        while (match(s, /"[^"]+"/)) {
          tok = substr(s, RSTART + 1, RLENGTH - 2)
          if (tok in S) k++
          s = substr(s, RSTART + RLENGTH)
        }
        if ($0 ~ /\]/) {
          inarr = 0
          if (k >= 2) printf "%s:%d: literal array names %d shared sensitive keys\n", FILENAME, start, k
        }
      }
    }' || true)"
  if [ -n "$restated" ]; then
    while IFS= read -r line; do
      bad "$line - iterate scrub::SENSITIVE_KEYS instead of restating it"
    done <<< "$restated"
  fi
fi

if [ "$failed" -gt 0 ]; then
  echo "sensitive-key agreement: $failed violation(s) against $keys shared keys at $rev"
  exit 1
fi
echo "sensitive-key agreement: ok ($keys shared keys at $rev)"
