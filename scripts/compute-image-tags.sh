#!/usr/bin/env bash
# Computes the per-build image tags and whether the signature should be
# recorded in the public Sigstore transparency log (Rekor).
#
# Rekor has no SLA. A testnet or main push happens many times a day and must
# not be blocked by a third-party transparency-log outage, so those sign with
# tlog_upload=false: a valid keyless signature from the GitHub OIDC identity,
# with no public log entry. A release tag is what a human verifies
# independently of this pipeline (see docs/src/security/verifying-releases.md
# and VERIFYING.md), so mainnet keeps tlog_upload=true and stays worth the
# outage risk.
#
# Usage: compute-image-tags.sh <github_ref> <github_ref_name> <github_sha>
# Writes sha_tag=, tag=, and tlog_upload= lines to stdout, in the format
# $GITHUB_OUTPUT expects.
set -euo pipefail

REF="${1:-}"
REF_NAME="${2:-}"
SHA="${3:-}"

[ -n "$REF" ] || { echo "::error::compute-image-tags.sh called with no ref" >&2; exit 1; }
[ -n "$REF_NAME" ] || { echo "::error::compute-image-tags.sh called with no ref name" >&2; exit 1; }
[ "${#SHA}" -ge 7 ] || { echo "::error::compute-image-tags.sh called with a sha shorter than 7 characters: '$SHA'" >&2; exit 1; }

# Two tags per build. The moving one (testnet-latest) is what humans pull;
# the per-commit one is never reused, so the previous build keeps a name
# after the next push and rolling back is a `compose up` instead of
# revert -> rebuild -> redeploy. 7 chars matches `git rev-parse --short`,
# i.e. the build_sha the server reports on /health - so a running container
# can be matched to the image it came from.
echo "sha_tag=sha-${SHA:0:7}"

if [[ "$REF" == refs/tags/v* ]]; then
  echo "tag=$REF_NAME"
  echo "tlog_upload=true"
else
  echo "tag=$REF_NAME-latest"
  echo "tlog_upload=false"
fi
