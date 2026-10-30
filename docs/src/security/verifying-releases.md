# Verifying Releases

random.cash signs its Docker images with [Sigstore cosign](https://docs.sigstore.dev/)
in keyless mode. No long-lived signing key exists: the GitHub Actions job's OIDC
identity is the signer.

**Only a release tag's signature is recorded in the public
[Rekor transparency log](https://docs.sigstore.dev/logging/overview/).** The
per-commit `sha-<short-sha>` images that testnet and `main` push on every
commit are signed without a Rekor upload: Rekor is a third-party service with
no SLA, and a push happens many times a day, so one outage there must not be
able to block it. A release tag is the artifact verified independently of this
pipeline, so it keeps the transparency-log entry and is worth that (rare)
outage risk — see [How it works](#how-it-works).

Images are the only signed artifact. See
[What is not signed](#what-is-not-signed) — it is listed explicitly so you know
what you are not getting.

## Quick start

Install cosign, then verify the image you intend to run.

### Docker images

```sh
TAG=sha-a1b2c3d   # or a release tag such as v0.1.0

cosign verify \
  --certificate-identity-regexp \
    "^https://github.com/randomcash/ethpayserver/\.github/workflows/ci\.yml@refs/(heads|tags)/.*$" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  ghcr.io/randomcash/ethpayserver:$TAG
```

Add `--insecure-ignore-tlog` for a `sha-*` tag — it has no Rekor entry to check
(see above). Drop it for a release tag, which still has one.

Two images are published per commit from this repository:

| Image | Reference |
|-------|-----------|
| Server (API + migrations) | `ghcr.io/randomcash/ethpayserver:<tag>` |
| EVM monitor | `ghcr.io/randomcash/ethpayserver/evmmonitor:<tag>` |

The checkout client used to be the third. It is now built and published by
[payserver-client](https://github.com/randomcash/payserver-client), which serves
every payserver rather than this one. It is signed the same way but under **its
own identity**, so the regexp differs:

```sh
cosign verify \
  --certificate-identity-regexp \
    "^https://github.com/randomcash/payserver-client/\.github/workflows/ci\.yml@refs/(heads|tags)/.*$" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  ghcr.io/randomcash/payserver-client:$TAG
```

The tag this server is **verified against** is pinned in `ops/client-image.pin`.
The frontend deploys independently, so that is not necessarily what is live.

Tags follow `sha-<short-sha>` (immutable, one per commit) or a release tag.
`<branch>-latest` moves and nothing should deploy from it.

### Verify by digest

Signatures are made against the image digest, never the tag — a tag is a movable
pointer, so signing one says nothing durable about the bytes you pull. `cosign
verify` resolves a tag to its digest before checking, so verifying by tag is
sound. To remove the resolution step entirely, verify and deploy by digest:

```sh
DIGEST=$(crane digest ghcr.io/randomcash/ethpayserver:$TAG)
cosign verify ... ghcr.io/randomcash/ethpayserver@$DIGEST
```

## How it works

1. CI builds the binaries and pushes the images.
2. Immediately after each push, the job resolves the image's digest and calls
   `cosign sign --yes`, using a short-lived GitHub OIDC token (`id-token:
   write`) — no key material exists to store or leak.
3. Cosign exchanges that token for a short-lived code-signing certificate from
   [Fulcio](https://docs.sigstore.dev/certificate_authority/overview/). On a
   release tag, it also records the signature in
   [Rekor](https://docs.sigstore.dev/logging/overview/)
   (`--tlog-upload=true`); on every other build — testnet and `main` push on
   every commit — it signs with `--tlog-upload=false` and skips Rekor, so an
   outage of that third-party service can't block a push that happens many
   times a day.
4. Signing runs inside the publish step, so a signing failure fails the step that
   pushed the image and the pipeline stops.

## What a signature proves

That the image was built and pushed by the `ci.yml` workflow in the named
repository, from a commit reachable from the branch or tag in the certificate.
For a release tag, also that this was publicly recorded in Rekor at signing
time; a per-commit `sha-<short-sha>` image has no Rekor entry, so verify it
with `--insecure-ignore-tlog` — the certificate and signature are checked in
full regardless, only the transparency-log lookup is skipped.

It does **not** prove the code is free of defects, that the commit was reviewed,
or that whoever pushed it was authorised. It binds artifact to pipeline.

## What is not signed

| Artifact | Status |
|----------|--------|
| Docker images | **Signed** — keyless cosign, GitHub Actions OIDC |
| Binary tarballs | **Not published.** Releases ship images only. Anything presented as a random.cash release binary did not come from this pipeline. |
| Git tags | **Not signed.** Tags are lightweight and carry no GPG signature. Verify the commit through the image signature instead. |

## Supply chain summary

| Layer | Mechanism |
|-------|-----------|
| Build | Per-commit immutable image tags |
| Container images | cosign keyless via GitHub Actions OIDC, signed by digest |
| Transparency | Sigstore Rekor public log — release tags only; per-commit builds skip it so a Rekor outage can't block a push |
| Frontend | Separately built, signed, and deployed on its own cadence; `ops/client-image.pin` records the tag this repo tested against |
