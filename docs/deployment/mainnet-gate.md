# Mainnet Gate: testnet to mainnet promotion

This document defines the procedure for promoting an ethpayserver release from
testnet to mainnet, including the manual approval gate, post-deploy
health verification, and rollback playbook.

## Strategy

**Deploy + post-deploy smoke + manual rollback.** agent8.cash runs on a single
VPS via docker-compose. There is no blue/green or canary infrastructure. Safety
comes from:

1. A human approval gate in CI before the mainnet deploy triggers.
2. A health gate that verifies the new image before declaring success.
3. A documented, single-command rollback path.

## Pre-promotion checklist

Before clicking the manual deploy button in CI for the `main` branch:

- [ ] All chain monitors are synced on testnet (`/health/deep` shows
      `data_fresh: true` and all RPCs report `ok`).
- [ ] RPC provider quotas are sufficient for mainnet traffic.
- [ ] All secrets are present in the mainnet `.env` file:
      `DATABASE_URL`, `REDIS_URL`, `EVMMONITOR_CHAIN_*_RPC_*`,
      `WEBAUTHN_RP_ID`, `WEBAUTHN_RP_ORIGIN`.
- [ ] Database migrations have been reviewed. Run `migrate_postgres` in
      dry-run mode if available, or inspect the pending migration files
      for destructive operations (column drops, table renames).
- [ ] The testnet smoke test (`scripts/smoke-prod.sh` against testnet)
      is green.
- [ ] No open MRs with the `do-not-merge` label targeting `main`.

## What triggers a deploy

`.github/workflows/ci.yml`, in GitHub Actions:

| job | runs when | deploys |
|---|---|---|
| `Notify Deploy (testnet)` | push to `testnet` | automatic |
| `Notify Deploy (mainnet)` | `refs/tags/v*` only | on the tag |

Mainnet takes release tags and nothing else — there is no manual-approval
button on a branch, and no `main`-branch deploy. Cut the tag with
`scripts/release.sh`; that is the approval step.

Both jobs POST a `repository_dispatch` to the private deploy repository, which
owns the deploy itself. A 202 from that API means the event was accepted,
not that anything deployed — which is what the health gate below is for.

## Post-deploy health gate

For **testnet**, the `Verify testnet deploy` job runs
`scripts/health-gate.sh` against
`https://testnet.agent8.cash/api/health/deep` after the dispatch, polling
for up to 900 seconds (`HEALTH_TIMEOUT`).

The `/api` prefix matters: `testnet.agent8.cash` serves the client, whose
SPA fallback answers `/health/deep` with HTTP 200 and a page of HTML. A
gate pointed at the bare host would pass against a server that never
restarted.

For **mainnet**, the deploy itself verifies: the private deploy repository's
mainnet workflow asserts database and Redis connectivity, waits for every chain
monitor to reach `connected` + `is_healthy`, and checks the WebAuthn relying
party both on the container and as the running server resolved it — then
records `.deployed-sha` only once all of that passes, so the rollback target
is never a build that came up broken.

Mainnet has not been deployed yet: nothing resolves at `pay.agent8.cash`, and
the only tag in the repository is `v0.1.0-alpha`, which the release filter
refuses. The first real release is the first exercise of that path.

To check a mainnet deploy by hand:

```bash
HEALTH_URL=https://pay.agent8.cash/api/health/deep \
EXPECTED_SHA=$(git rev-parse --short=7 HEAD) \
HEALTH_TIMEOUT=900 ./scripts/health-gate.sh
```

The gate passes when ALL of the following are true:

1. `/health/deep` returns HTTP 200.
2. `build_sha` in the response matches the commit being deployed
   (`EXPECTED_SHA`, the first 7 of `GITHUB_SHA`).
3. The `x-sentry-release` response header matches `build_sha`. The two are
   set by separate CI steps from the same commit sha, so they can drift
   apart (a rename, a typo, a rebuild stage that drops the env var) without
   either build step failing — this catches that on the deployed binary,
   not the build log.
4. Postgres reports `status: "ok"`.
5. Redis reports `status: "ok"`.
6. All RPC chains report `status: "ok"` (no chain in error, disconnected,
   or connecting state).

If the gate does not pass within the timeout, the job fails. Because
Docker Compose keeps the old container running until the new one passes
its own health check, a failed gate means the old version is still
serving traffic — no rollback is needed in this case.

A SHA-mismatch timeout logs which of three situations it saw, since they
call for opposite responses: no response ever reported a `build_sha`
(endpoint unreachable, or the deploy hasn't started responding at all — wait
and check again), the sha never moved from what was running before the
poll started (the new build hasn't landed yet — wait), or the sha changed
but to something other than `EXPECTED_SHA` (something is actually wrong —
investigate). Only the last of these means the deploy itself is broken.

### Post-deploy smoke test

There is no automated smoke job in `ci.yml` — after the health gate passes,
an operator runs `scripts/smoke-prod.sh` by hand against the deployed
instance:

- `/health/live` — process is running
- `/health/ready` — DB, Redis, and all RPC chains reachable
- `/health/deep` — detailed dependency check
- Invoice create/read cycle via API key
- Checkout page load for the created invoice

### Environment variables for the scripts above

| Variable | Used by | Description |
|----------|---------|-------------|
| `HEALTH_URL` | `health-gate.sh` | Full URL to `/health/deep` on the target env |
| `HEALTH_TIMEOUT` | `health-gate.sh` | Seconds to poll before failing (default 60) |
| `EXPECTED_SHA` | `health-gate.sh` | Commit the deploy should be serving, first 7 of the sha. Unset means gate condition 2 is skipped, so the gate passes against a server that never restarted — set it |
| `SMOKE_BASE_URL` | `smoke-prod.sh` | Base URL for smoke tests (e.g. `https://pay.agent8.cash`) |
| `SMOKE_API_KEY` | `smoke-prod.sh` | API key with invoice create/read permissions |
| `SMOKE_STORE_ID` | `smoke-prod.sh` | Store UUID the smoke API key is scoped to |

## Container registry tagging

Every CI pipeline tags container images with both the short commit SHA
and a branch-latest tag:

```
ghcr.io/randomcash/ethpayserver:sha-<short_sha>
ghcr.io/randomcash/ethpayserver:<branch>-latest
```

The SHA tag is immutable and deterministic — it is the tag used for
rollback.

## Rollback procedure

If mainnet is broken after a deploy, rollback to the previous known-good
image:

### 1. Identify the previous good SHA

```bash
# On the VPS, check which image was running before:
docker inspect ethpayserver_server --format='{{.Config.Image}}'
# Or take the short SHA of the last release tag that deployed cleanly:
#   gh run list --workflow ci.yml --limit 20 --json headSha,conclusion,headBranch
```

### 2. Retag and redeploy

```bash
# SSH into the VPS
cd /path/to/ethpayserver/docker

# Update the image tag in .env or docker-compose override:
export SERVER_IMAGE=ghcr.io/randomcash/ethpayserver:<previous_sha>
export MONITOR_IMAGE=ghcr.io/randomcash/ethpayserver/evmmonitor:<previous_sha>
# The frontend is versioned separately and does not follow <previous_sha>.
# Use the tag pinned at that commit: git show <previous_sha>:ops/client-image.pin
export CLIENT_IMAGE=ghcr.io/randomcash/payserver-client:<pinned_tag>

# Pull and restart
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
```

### 3. Verify the rollback

```bash
curl -s https://pay.agent8.cash/api/health/deep | python3 -c \
  "import json,sys; d=json.load(sys.stdin); print(f'sha={d[\"build_sha\"]} pg={d[\"postgres\"][\"status\"]} redis={d[\"redis\"][\"status\"]}')"
```

Expected output: `sha=<previous_sha> pg=ok redis=ok`

### 4. Revert the commit on main

```bash
git revert <bad_commit_sha>
git push origin main
```

This prevents the bad commit from being accidentally re-deployed on the
next pipeline run.

## Build SHA verification

The `/health/deep` endpoint exposes a `build_sha` field that contains the
short commit SHA baked into the binary at compile time. This allows:

- The health-gate script to confirm the new version is actually running.
- Operators to quickly confirm which version is live.

The same response also carries an `x-sentry-release` header — the value
compiled into the binary via `option_env!("SENTRY_RELEASE")`, i.e. what this
process hands Sentry as its `release` tag. `build_sha` and the Sentry
release are set by separate CI steps from the same commit sha and can drift
apart without either step failing, so the health gate compares them on the
running process rather than trusting that the build succeeded.

evmmonitor is a second binary that tags its own Sentry events from the same
`SENTRY_RELEASE`, compiled in its own CI step, and has no HTTP endpoint of
its own to check directly. When it's configured, the response carries its
compiled release too, relayed through the same Redis channel evmmonitor
already reports chain health over, as `x-evmmonitor-sentry-release`. The
health gate compares that against `build_sha` the same way, so a drift in
evmmonitor's build step is caught on the deployed process as well.

```json
{
  "build_sha": "abc1234",
  "version": "0.1.0",
  "postgres": { "status": "ok", "latency_ms": 3 },
  "redis": { "status": "ok", "latency_ms": 1 },
  "rpcs": { "1": { "status": "ok", "latency_ms": 12, "last_block": 20000000 } },
  "monitor": { "status": "ok", "data_fresh": true }
}
```
