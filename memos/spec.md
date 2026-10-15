# What this is

A non-custodial EVM payment processor. A merchant registers an extended public
key; the server derives a fresh receiving address per invoice from it, watches
that address on chain, and records payments as they confirm. **The server never
holds a spending key and cannot move funds.**

That guarantee is load-bearing rather than aspirational. `validate_xpub` accepts
only a base58 `xpub` and refuses an `xprv` on the version-byte prefix, so a
merchant cannot hand over a spending key even by pasting the wrong line.
Anything that would require the server to hold one is a change to what this
product *is*, not a feature.

## Shape

```
payserver-commons     shared types, auth, crypto, rates, ui-kit
       |  pinned BY REVISION in Cargo.toml
       v
this repository       API, monitor, data-service
       |  pins a published client image in ops/client-image.pin
       v
payserver-client      frontend, its own repository
```

The client never depends on a payserver — it talks to whichever one is
configured at runtime. Do not add a dependency edge from client to server.

`memos/features.json` is no longer part of the session loop and is not kept
current. It is retained because its open rows (`passes: false`) carry design
constraints that live nowhere else, for example that a stale watch is reported
and never auto-removed, and that the reconciler diff key is the full tuple, not
the address. Check any row against the code and open pull requests before acting
on it. Its `passes` values are a frozen snapshot from 2026-09-26, not a status
board: a `false` does not mean the work is outstanding. F004 and F005 describe
queue and worker automation that has since landed in the private deploy
repository, so they are not work for this one, and F001 and F009 are
reconciler work whose state is in the code and the open pull requests. Only the
design constraints in the notes are still wanted.

## What works today

A snapshot, measured on 2026-09-26 at `142d923` and not kept current — rerun
the gate for today's numbers:

- `cargo build --workspace` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — **656 passed, 0 failed, 210 ignored**

The 210 need a live Postgres and Redis. They are not optional extras: CI runs
them with `--run-ignored` and they gate merges. They include the cross-tenant
isolation suite, which was verified by ablation on 2026-09-26 — the tenant guard
in `get_store_wallet` was disabled deliberately and exactly two assertions went
red, then green again when restored. That suite genuinely exercises what it
claims to.

End-to-end tests run against a real server and the **pinned** client image, so
the pin can silently fall behind and a feature can ship in the client while every
test runs against a build that predates it.

## Known weak points

These are recorded because a session that rediscovers one wastes a day.

- **A check can pass without having looked.** Several here have. Before trusting
  a green, ask what it measured and against what.
- **The client is pinned.** When the client renames a class, the selectors and
  the pin bump must move in the same commit, or each half looks broken to
  whoever touches it next.
- **Rate limits fail the end-to-end suite for the wrong reason** — the limiter
  returns 429 and logs nothing, so the server looks healthy while tests fail in
  no pattern.
- **Deploy is not merge.** A merge can succeed while its deploy fails, leaving
  the branch ahead of the running image. Verify with the health endpoint and a
  build-sha comparison, not with the merge result.

## Questions

`QUESTION:` marks something a session must not guess at. An `ANSWERED:`
entry is a decision that has been made — treat it as binding and do not
re-open it, but do read the reasoning, because it usually constrains more than
the question asked.

**ANSWERED 2026-09-26: both, and delivery is measured as well.** Reliable
operation on testnet *and* readiness to provision a production instance — the
question offered them as alternatives and the answer refused the choice. So
correctness work on what exists does not get to postpone the deploy and backup
paths, and the reverse also holds.

Progress is measured in **features delivered**, tracked as tickets in the
private tracker, not in commits, green checks or refactors. A session that ends
with a cleaner tree and no delivered feature has not moved this. The practical
consequence for anything read here: when two pieces of work both look
worthwhile, prefer the one that closes a ticket end to end over the one that
improves something already working.

**ANSWERED 2026-09-26: no**, and the guard was narrowed to match. Recorded here because the reasoning is not obvious from the code: a
payment already *detected* still blocks deletion, via the payments-row check,
and that is the case worth refusing. "Unpaid" and "in flight" are different
things.

**ANSWERED 2026-09-26, then SUPERSEDED the same day: the gate no longer
blocks.** Measurement is what settled it — nine of sixteen open pull requests
could not pass the line limit, and clearing them by hand meant splitting eight
production files, several on money paths, to satisfy a line count. The decision
was that this code's only readers are agents, so a readability rule is not worth
blocking merges for. `scripts/check-file-size.sh` now reports and warns instead
of failing; `ENFORCE_FILE_SIZE=1` arms it again.

**So the report is the whole mechanism now.** The curve that prompted the gate
was 26 oversized files becoming 47 in nine days while split tickets were
landing — the splitting happened and the growth outpaced it, because nothing
checked. Expect that to resume. Whoever reads a green `check.sh` should read its
file-size line rather than the exit code.

The ownership answer below still stands for the backlog itself, and its
reasoning is why the sequencing question was the hard part rather than the
limit:

**The overseeing session owns it.** Not the feature
sessions, and not whoever happens to trip the gate next — sequencing it is the
owner's job, because the cost is in the order the splits land rather than in any
single split.

The reason it needs an owner rather than a rule is visible in what happened
after the first batch of splits landed: a split does not clear a blocked pull
request, it **converts a gate failure into a merge conflict**, and every branch
still holding the pre-split file now has to be re-homed into the new child
modules. Four branches currently conflict on the same split file, which means
they also conflict with each other and cannot be resolved independently — they
need an order, and the second one through is cheap only if the first one through
is already merged. Splitting a file that no open branch touches is nearly free;
splitting one that four branches touch is the most expensive thing available.
Sequence by how many open branches hold the file, not by how far over the limit
it is.

**ANSWERED 2026-09-26: it should not happen, and there must be a failsafe.**
Divergence is not an operating condition to be reported and tolerated; detection
(now built) is the floor, not the answer.

**The failsafe must be directional, because the two divergences are not
symmetric.** A watch the database expects and nothing holds means a payment can
arrive and be credited to nobody — and re-establishing it is idempotent and
costs a little work. A watch held that the database does not expect wastes a
resource, and removing it is an external side effect with **no rollback**: act
on a comparison that was wrong in that direction and you stop watching an
address about to be paid into, converting a wasted resource into a lost payment.

So: **re-establish missing watches automatically; never remove a stale watch
automatically.** Report the stale ones for a person. This is the same asymmetry
that governs the deletion path — the recoverable direction may be automated, the
irreversible one may not.

One known benign case must not trip the failsafe: cleanup unwatches before it
deactivates the database row, so between those two steps a watch reads as
missing while everything is behaving correctly. Such an entry belongs to an
invoice that is no longer pending, which is how it is told apart from the case
that costs a payment.
