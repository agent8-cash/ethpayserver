# Operations Runbook

## Health Endpoints

ethpayserver exposes three health probe endpoints, none of which require authentication. They do **not** query invoice or payment tables — they only check infrastructure dependencies.

### `GET /health/live`

**Purpose:** Kubernetes/systemd liveness probe. Confirms the process is alive.

- Always returns **200 OK** if the server is running.
- If this fails, the process is unresponsive — restart it.

### `GET /health/ready`

**Purpose:** Kubernetes/systemd readiness probe. Gates traffic during rolling restarts.

Returns **200** only when **all** of the following respond within 1 second:

| Dependency | What is checked |
|------------|-----------------|
| Postgres   | `SELECT 1` ping |
| Redis      | `PING` command  |
| RPC chains | Each configured chain reports `is_healthy` via evmmonitor health data in Redis |

**200 response:**

```json
{"status": "ready"}
```

**503 response** (with list of failing dependencies):

```json
{"status": "not_ready", "failing": ["postgres", "rpc:56"]}
```

Use this endpoint for load-balancer health checks and deployment readiness gates.

### `GET /health/deep`

**Purpose:** Operator diagnostic endpoint. Not intended for load-balancer decisions — use `/health/ready` for that.

Always returns **200** with a JSON body containing per-dependency status and latencies:

```json
{
  "build_sha": "a1b2c3d",
  "version": "0.1.0",
  "postgres": {"status": "ok", "latency_ms": 3},
  "redis": {"status": "ok", "latency_ms": 1},
  "rpcs": {
    "1":  {"status": "ok", "latency_ms": 5, "last_block": 20000000},
    "56": {"status": "error", "latency_ms": 1000, "error": "disconnected"}
  },
  "monitor": {"status": "ok", "data_fresh": true}
}
```

The response also carries an `x-sentry-release` header with the compiled
`SENTRY_RELEASE`, so a deploy check can compare it against `build_sha` from
the same running process — see `docs/deployment/mainnet-gate.md`.

| Field | Description |
|-------|-------------|
| `build_sha` | Short commit sha baked in at compile time. What `deploy-verify-testnet` and the mainnet gate poll for to confirm a deploy landed. |
| `version` | `CARGO_PKG_VERSION` of the running binary |
| `postgres.latency_ms` | Round-trip time for a Postgres `SELECT 1` |
| `redis.latency_ms` | Round-trip time for a Redis `PING` |
| `rpcs.<chain_id>.last_block` | Latest block number reported by evmmonitor for this chain |
| `rpcs.<chain_id>.error` | Present only when the chain is unhealthy |
| `monitor.data_fresh` | `true` if evmmonitor has published health data to Redis |

### Existing endpoints with admin-gated detail

- `GET /health/chains` — per-chain health from evmmonitor. Public: chain
  identity and up/down only. With a `Bearer` token carrying server admin
  privileges: block heights, watched-address counts and failure reasons too
  (same underlying data as `/health/deep`'s RPCs section).
- `GET /metrics` — Prometheus exposition format for scraping. Admin-only, no
  unauthenticated response at all.

## Watching `/health/deep` between deploys

A 200 from `/health/live` or `/health/ready` does not mean payments are being
detected — the process can be up while `monitor.data_fresh` is false or an RPC
has gone quiet, and a check that only reads the status code stays green
through that. Two things exist so far that touch `/health/deep`, and both are
deploy-triggered, not continuous:

- `scripts/health-gate.sh`, run by `deploy-verify-testnet` in `ci.yml` on every
  push to `testnet`, and manually against mainnet per
  `docs/deployment/mainnet-gate.md`. Gates a rollout; does nothing once the
  rollout has succeeded.
- `scripts/smoke-prod.sh`, run manually per the same checklist.

Between deploys, coverage is `.github/workflows/health-monitor.yml`: it runs
`scripts/check-health-deep.sh` against both `testnet.random.cash` and
`pay.random.cash` every 5 minutes on a GitHub-hosted runner, asserts
`postgres`, `redis`, `monitor.data_fresh` and every `rpcs.*.status`, and checks
in to a Sentry Cron Monitor (`SENTRY_CRON_HEALTH_TESTNET_URL` /
`SENTRY_CRON_HEALTH_MAINNET_URL`), intended so that a missed or failing check pages through
Sentry's alerting rather than sitting unread in the Actions tab (but see the
next paragraphs: most of those monitors are currently disabled). The scheduled
e2e workflow's dead-man's-switch moved onto the same vendor, checking in to
`SENTRY_CRON_E2E_URL`.

**What actually alerts, as of 2026-10-11.** Cron Monitors cost a paid seat
each, and the project does not have seats for all of them. Of the four, three
are **disabled**: `flow-monitor`, `health-testnet` and `db-backup-testnet`.
Only the end-to-end dead-man's-switch (`e2e-testnet`) is live. So a failing or
missing `health-monitor.yml` check-in, and a missed database backup, **do not
page anyone through Sentry today**; the red Actions run is the only trace, and
nobody is watching that tab. The host timer described below is the intended
path for prompt outage detection, but it is unverified from this repository,
so do not assume any alarm reaches a person.

A check-in to a disabled monitor still returns success. `flow-monitor` has
never recorded a check-in at all: the `http=202` it receives means Sentry's
ingest endpoint accepted the request, not that a check-in landed on a monitor.
Do not read a 2xx from a check-in URL as proof an alarm is wired.

Until 2026-10-11 the project's alert rule fired once per issue and never
again, so `e2e-testnet` sat in a failed state for 18 days without a second
notification. The rule now re-notifies; if you change alert rules, re-check
that a still-failing monitor keeps alerting.

The scripts still require the URLs (`SENTRY_CRON_HEALTH_TESTNET_URL`,
`SENTRY_CRON_HEALTH_MAINNET_URL`, `SENTRY_CRON_E2E_URL`) and fail when one is
unset, rather than skipping the check-in: a watchdog that quietly skips its
report would pass while paging nobody. That requirement is about the code, and
it is not evidence an enabled monitor sits behind each URL. Re-enabling a
monitor needs a paid seat and a person with Sentry dashboard access, which
this repository does not have.

`rpcs.*.status` alone misses a chain whose indexer has wedged while the RPC
connection itself stays up — `status: ok` with `last_block` frozen. Each job
restores `.health-state/<env>.json` from an `actions/cache` entry keyed on the
environment (an ordinary GitHub Actions run has no other persistence between
schedule ticks), passes it to `check-health-deep.sh` as `HEALTH_STATE_FILE`,
and saves it back afterwards regardless of pass/fail. The script tracks how
many consecutive checks a chain's `last_block` has repeated and fails once
that exceeds `STALL_THRESHOLD` (default 3 checks — about 15-20 minutes flat
only if every 5-minute run is dispatched, which GitHub does not do; at the
delivered cadence it is many hours. See the next paragraph).

Five minutes is GitHub Actions' practical floor, and it is a floor, not a
delivery guarantee. Scheduled workflows are best-effort: this one has been
observed dispatching only a small fraction of its runs, with gaps of hours, and
that figure will drift. The `*/5` cron therefore does not hold a 5-minute
cadence.

**Where the 5-minute cadence actually comes from.** Not from this workflow.
The deploy repository (`central-infrastructure`, private) defines a
one-minute systemd timer that polls `/api/health/deep` and, per the script's
own header as read when this was written (re-read it before relying on this),
reports success to a dead-man's switch and treats a chain whose block has not
advanced within `STALE_BLOCK_SECS` (default 600) as down, so a down service,
a broken watcher and a dead box all surface as a late check-in. That is the
intended path for prompt outage detection. The timer's
unit and script are `systemd/rcs-health-watch@.timer` (`OnUnitActiveSec=1min`)
and `scripts/health-watch.sh` in that repository, deliberately:
they depend on host paths and credentials that must not be published here.
Whether `rcs-health-watch@testnet.timer` is installed and enabled is
**unverified from this repository**: enabling it is the deploy-repository
owner's job, and it can be checked with `systemctl list-timers` on the host.
Until that is confirmed, treat prompt outage detection as not yet in place.

Consequences for this workflow's Sentry Cron Monitors: their check-ins come
only from the GitHub schedule, so a window sized for 5 minutes alarms
constantly, and one widened to match delivery cannot tell "testnet is down"
from "GitHub skipped runs". Size the monitors' expected interval to hours and
treat them as a backstop for the workflow itself, not as outage detection.
Resizing the monitors is a manual Sentry change owned by the operator.
`STALL_THRESHOLD` counts checks, not minutes, so it stretches the same way.
It is deliberately left at 3 rather than lowered: a lower value would
false-flag a slow chain on the 5-minute cadence the workflow is nominally on,
and at the delivered cadence of hours it cannot be made prompt anyway. A
flat chain is therefore flagged here only after several delivered runs; the
host timer's seconds-based stale-block check (if enabled) is what bounds
stall time. The check-in needs no code change at this cadence: it fires once
per delivered run, so the only adjustment is the monitor window in Sentry. The scheduled end-to-end workflow is subject to the same
best-effort dispatch and is a separate problem, not addressed here. The two
Sentry Cron Monitor URLs above have to be created by hand in Sentry (Crons →
new monitor → "check in via HTTP") and the resulting URLs stored as repo
secrets; that account setup is outside what a commit here can do.

For the faster cadence, also add a Sentry **Uptime Check** (not a Cron
Monitor) against `/api/health/deep`, run from Sentry's own checkers at 30-60s.
As of this writing it was not confirmed whether Sentry's uptime check can
assert on the response body rather than just the status code — if it can,
point its alert at the same field failures; if it can't, the workflow above is
the fallback that actually reads the body, and is why it stays regardless of
what the uptime check can do.

## Editing a migration that has already run

`sqlx` checksums the **whole migration file** — SHA-384 of its bytes — and
records it in `_sqlx_migrations`. On the next run it compares, and refuses with
`VersionMismatch` if the file has changed. A one-word comment edit is enough.
Because `migrate` runs before `server` in the deploy, that failure keeps the
whole stack down.

Two facts worth knowing before you touch one:

- **Renaming a migration file is free.** The version is the numeric prefix and
  the description is stored but never compared, so only the bytes matter.
- **Editing its contents is not**, whether or not any statement changed.

When an edit is deliberate and the migration has already run somewhere, refresh
the recorded checksums rather than resetting the database:

```sh
# 1. Keep the current values — see "this is one-way" below.
psql "$DATABASE_URL" -c "\copy (SELECT version, encode(checksum,'hex') \
  FROM _sqlx_migrations ORDER BY version) TO 'checksums-before.csv' CSV"

# 2. Look at what is recorded.
psql "$DATABASE_URL" -c 'SELECT version, description FROM _sqlx_migrations ORDER BY version'

# 3. Apply.
psql "$DATABASE_URL" -f ops/refresh-migration-checksums.sql
```

`ops/refresh-migration-checksums.sql` re-records checksum and description per
version. It touches no schema and no data, and matches nothing on a database
where those migrations have not run. Regenerate it whenever a migration file's
bytes change.

No `-1`: the script carries its own `BEGIN`/`COMMIT` and sets
`ON_ERROR_STOP`. Without `ON_ERROR_STOP`, psql runs on past a failed `UPDATE`,
the enclosing transaction is already aborted, `COMMIT` degrades to a rollback —
and **psql still exits 0**, so a refresh that did nothing reports success and
the deploy goes down anyway.

### This is one-way

`sqlx::migrate!` bakes checksums into the binary at compile time, so an image
built before the edit carries the old ones. After the refresh, that image's
`migrate` exits `VersionMismatch` and `server` never starts behind it — so
**rolling back to any earlier image stops working**, and rollback is otherwise
just a `compose up` with an older tag. Run the refresh only alongside deploying
an image built from a tree that contains the edited migrations, and keep
`checksums-before.csv` if you might need to go back.

### Every environment, not just the one in front of you

Run it wherever the database has those migrations applied. Nothing in CI does it
for you, and `main` dispatches testnet while tags dispatch mainnet, so an
edit that only got refreshed on testnet takes the next environment down at the
first already-applied version it reaches.
