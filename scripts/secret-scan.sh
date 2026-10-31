#!/usr/bin/env bash
# Scans the commits between a base commit and HEAD for committed secrets.
# Exits 1 on a finding, 0 when clean. Needs the scanner on PATH; a missing
# scanner exits non-zero too, so the caller must not read that as "clean".
# Usage: secret-scan.sh <base-sha>
set -uo pipefail

base="${1:?usage: secret-scan.sh <base-sha>}"
exec gitleaks git --no-banner --redact --exit-code 1 --log-opts="$base..HEAD" .
