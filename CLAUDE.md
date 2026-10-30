# ethpayserver

A non-custodial EVM payment processor. Merchants receive crypto directly to
addresses derived from their own extended public key; the server never holds a
spending key and cannot move funds.

That guarantee is load-bearing. `validate_xpub` accepts only a base58 **xpub** —
an `xprv` is refused on the version-byte prefix — so a merchant cannot hand over
a spending key even by pasting the wrong line. Custody — hot wallets held by
the server, and connection to cold/air-gapped signing — is on the roadmap, but
is not in beta and nothing in the current code should assume it.

## This repository is public

So are `payserver-commons` and `payserver-client`. Consequences that are easy to
forget:

- **Never commit a reproduction for an unfixed vulnerability.** Describe the fix
  and the property now enforced; the exploit belongs in the private tracker.
- No session URLs in commits or PR bodies.
- No secrets, obviously — but also no internal hostnames, no private paths.
- **No ticket ids or tracker links in source files** — no `RCS-123`, no
  `linear.app` URL, in code, comments, doc comments or test names. A ticket id
  here leaks the title and shape of unreleased work to anyone reading, and it
  is useless to the only audience the code has: an outside reader cannot open
  it. Write the reason the code exists, not a pointer to where someone once
  explained it. The ticket id belongs in the commit message and the PR title,
  both of which already carry it. `scripts/check-no-ticket-refs.sh` enforces
  this in CI; `CLAUDE.md`, `AGENTS.md` and `docs/` are exempt.

Deploy config and billing logic live in private repositories and must not
migrate here.

## Three repositories, one product

```
payserver-commons     shared types, auth, crypto, rates, ui-kit
       |  pinned BY REVISION in Cargo.toml
       v
ethpayserver          this repo: API, monitor, data-service
       |  pins a published client image in ops/client-image.pin
       v
payserver-client      Leptos/WASM frontend, its own repository
```

**The client never depends on a payserver.** It talks to whichever one is
configured at runtime. Do not add a dependency edge from client to server.

### Changing commons is a three-step dance

`payserver-commons` is pinned by `rev` in the workspace `Cargo.toml`, not by
branch. A change there does not reach this repo until the pin moves.

1. change + merge in commons
2. bump the `rev` here (all crates share one revision), `cargo update -p …`
3. only then does the code see it

To work against a local commons checkout use `scripts/commons.sh link`. **Run
`scripts/commons.sh unlink` before verifying or committing** — a live link builds
against your working copy, so a green build proves nothing about the pin.

## The gate

Exactly what CI's `lint` and unit-test steps run (`.github/workflows/ci.yml`)
— the `test` job also runs a separate, gating integration-test step against a
real Postgres instance; see "End-to-end tests" below for that one:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --no-fail-fast -j 2
```

`--workspace` without `--lib` on purpose: the integration binaries under
`evm/tests/` and `server/tests/` used to be compiled by clippy and never run,
so they could sit red indefinitely while the gate reported green. Five of them
were, on 2026-09-16.

**Do not add `--all-features`.** It surfaces pre-existing errors in `evmmonitor`
that are not in CI's path. Several people have lost an hour to this.

## End-to-end tests

`e2e/` is Playwright against a real server and a real client image.

- **The client is pinned** in `ops/client-image.pin`, and CI tests against that
  pin rather than the deployed client. So the pin can silently fall behind, and
  a feature can ship in the client while every test still runs against a build
  that predates it.
- **When the client renames a class, the e2e selectors and the pin bump must
  move in the same commit.** Split apart, one half looks for controls the other
  half no longer labels that way — and whoever bumps the pin next inherits
  failures they did not cause.
- **Rate limits will fail the suite for the wrong reason.** Defaults
  (`server/src/api/rate_limit.rs`) are `auth_rpm: 30`, `write_rpm: 120`. A full
  run makes far more than 120 writes a minute, and the limiter returns 429
  **without logging anything** — so the server looks healthy while tests fail
  in no pattern. Run a local server with every `RATE_LIMIT_*` at `10000`, as
  CI does. See `e2e/README.md`.
- Integration tests are `#[ignore]` by convention and need `DATABASE_URL`. A
  few also need a real Redis and gate on `TEST_REDIS_URL` the same way,
  skipping (not failing) when it is unset - CI sets both for the job that runs
  `--run-ignored`, so don't take an unset `TEST_REDIS_URL` locally as proof a
  Redis-dependent test can't fail; it's provisioned in CI even when it isn't
  on your machine. CI *does* run them — the `test` job migrates a real
  Postgres service, starts a Redis service, and runs
  `cargo nextest run -p data-service -p server --no-fail-fast --run-ignored only -j 1`
  with both env vars set — so a failure there gates merges same as any other
  test. The `-j 1` is not cosmetic: these tests share one real Postgres
  instance, so run them locally with the same flag rather than nextest's
  default concurrency, or you can get spurious cross-test failures CI never
  sees. The command now covers `data-service` **and** `server`, so a
  `server/tests/*.rs` integration test does gate merges - this paragraph used
  to say otherwise, which is worth knowing if you wrote one and assumed it
  never ran. Other crates' `#[ignore]`'d tests are still not in it and need
  running locally. The gate above does not touch any of them either way, so run
  the `data-service` ones locally too when you touch that layer — CI will
  catch a failure regardless, but locally you see it sooner.
- A separate `test` job step runs `evm`'s `#[ignore]`'d Redis integration
  tests (`cargo nextest run -p evm --features redis --test redis_bridge
  --run-ignored only -j 1`, against a `redis:7-alpine` service) — the `redis`
  feature is off by default, so this is the only place
  `evm/tests/redis_bridge.rs` ever compiles, let alone runs. That step also
  gates merges. It names `--test redis_bridge` rather than building all of
  `-p evm`'s tests: `evm/tests/payment_flow.rs` and `rpc_cost.rs` do not
  compile scoped to `-p evm` alone (only under `--workspace`, where feature
  unification with `server` happens to fix it) - naming the one binary this
  step needs avoids tripping over that.

## Security- and funds-critical code

Auth, crypto, on-chain interaction, wallet/key handling, migrations, and
anything touching movement of funds deserve extra care.

Work in it normally, but say so prominently in the commit message.

## Conventions that bite

- **`git grep`, not bare `grep`.** `grep` here is `ugrep` and honours
  `.gitignore`, so a plain recursive grep silently skips files. That has produced
  false "clean" results on leak scans.
- **Never `git add -A`.** Check `git status --short | grep '^??'` first and add
  paths explicitly. Build artefacts and symlinked `node_modules` have nearly been
  committed this way.
- **Editing a migration changes its checksum.** `sqlx` stores a SHA-384 of the
  whole file and compares it on startup, so editing a *comment* in an applied
  migration breaks every deploy. Renaming the file is free; changing its bytes is
  not.
- **A test that cannot fail is worse than no test.** Before trusting one, break
  the thing it covers and confirm it goes red. Several tests here have passed for
  the wrong reason — an endpoint that 401s regardless of state, a duplicate-id
  case that fails at the first statement so there is nothing to roll back.
- **A unit test does not prove the feature is reachable.** Test the thing
  through the entry point a user or a caller actually reaches it by. This repo
  has shipped fully tested code wired to nothing more than once:
  `scripts/health-gate.sh` sat in the tree unreferenced by any CI job until
  `deploy-verify-testnet` finally called it, and `services/plugins/core_data.rs`
  (151 lines, referenced only by its own re-export) was later deleted outright
  as dead code. `api::plugins::router()` (nine passing tests) is still mounted
  in no router today. Every one had green tests. `pub` is not reachability —
  Rust's `dead_code` lint says nothing about an exported item nothing imports.
- **Two migrations must never share a version.** sqlx keys applied migrations
  by the number in the filename; the second file to claim one is refused on
  every startup from then on, not just the first, and recovering means editing
  the database by hand. Branches opened the same day collide easily and each is
  green alone. `scripts/check-migrations.sh` enforces this in CI. Renaming a
  migration is free; editing an applied one's bytes is not.

## Deploys

`testnet` deploys on every push, via a dispatch to the private deploy repository.
`mainnet` takes release tags only — `vMAJOR.MINOR.PATCH` exactly, no prerelease
suffix — and holds real merchant funds. There is no staging.
- **A bound that logs its measurement every time it checks produces a time
  series for free, and the series answers questions the bound was not built
  to answer.** When a threshold fires or a number looks wrong, pull the last N
  readings before reasoning about the latest one. A level is not a rate (26G
  against a 30G ceiling says nothing about how fast the gap is closing), and
  three widely spaced points straddling the steps of a staircase look exactly
  like a ramp. One decrease falsifies unbounded growth; twenty increases are
  consistent with both. `scripts/check-file-size.sh` prints a per-file report on
  every CI run and no longer blocks, so those runs are the only record of the
  growth curve.
