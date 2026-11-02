#!/usr/bin/env bash
# Reads changed paths on stdin and prints the ones in a sensitive area: auth,
# crypto, the EVM crate, wallet and key derivation, database migrations, the
# invoice, payment, payout and refund code, and secrets, env files and RPC
# configuration. Prints nothing when no path qualifies. Always exits 0; the
# caller decides what a hit means. A hit is a review request, not a failure.
set -uo pipefail

PATTERN='^evm/'
PATTERN+='|(^|/)(auth|crypto)[^/]*(/|\.rs$)'
PATTERN+='|^data-service/migrations/'
PATTERN+='|(^|/)(wallets?|hd[_-]?wallet|derive[^/]*)([/_.-]|$)'
PATTERN+='|(^|/)(invoices?|payments?|payouts?|refunds?)[^/]*(/|\.rs$)'
PATTERN+='|(^|/)secrets?([/_.-]|$)|(^|/)\.env'
PATTERN+='|(^|/)rpc[^/]*(/|\.rs$)'

grep -E "$PATTERN" || true
