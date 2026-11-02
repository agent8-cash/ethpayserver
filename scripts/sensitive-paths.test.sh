#!/usr/bin/env bash
# Proves sensitive-paths.sh flags the areas a human must review and leaves
# ordinary paths alone. A flag that matches nothing passes every run, so the
# positive cases are the point.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLAG="$HERE/sensitive-paths.sh"
fail=0

check() { # expected(hit|none) path
    local got
    got="$(printf '%s\n' "$2" | "$FLAG")"
    if [ "$1" = hit ] && [ -z "$got" ]; then echo "FAIL: not flagged: $2"; fail=1; fi
    if [ "$1" = none ] && [ -n "$got" ]; then echo "FAIL: wrongly flagged: $2"; fail=1; fi
}

for p in evm/src/lib.rs \
         server/src/api/invoices/create.rs \
         server/src/api/users/wallets.rs \
         data-service/migrations/postgres/0042_add.sql \
         data-service/src/postgres/auth/sessions.rs \
         server/src/services/plugins/payout.rs \
         server/src/api/refunds/mod.rs \
         server/src/services/secrets.rs \
         ops/.env.production \
         evm/src/monitor/chain/rpc.rs; do
    check hit "$p"
done

for p in README.md \
         scripts/check.sh \
         ui/src/components/Button.tsx \
         docs/architecture.md; do
    check none "$p"
done

[ "$fail" -eq 0 ] && echo "sensitive-paths self-test: ok"
exit "$fail"
