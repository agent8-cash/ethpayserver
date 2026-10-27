# agent8.cash — ethpayserver

agent8.cash is a self-hosted, multi-chain crypto **payment processor** (Bitcoin + EVM). This repo,
`ethpayserver`, is the EVM server. Its siblings: `payserver-commons` (shared code, **pinned by
revision**), `payserver-client` (the testnet WASM client), `payserver-billing`, and
`central-infrastructure` (deploy/ops + the authoritative automation contract, `AUTOMATION.md`). Stack:
Rust + Leptos. **Real money runs through this — no change here is "just a demo."** The integration
branch is `testnet`; `main` (and a future payserver's `main`) are the only mainnet deploy paths.

You are one agent working one ticket. Read this, then `memos/spec.md`, `git log --oneline -20`, and the
ticket you were given. There is no session log: the tickets and the commit history carry that context.

## The loop

0. **Search the open pull requests for your feature's subject before writing
   anything.** Search the subject, not a ticket id - a harness feature does not
   have one. A feature already in flight looks exactly like a feature nobody has
   started, and the difference is a day of work. This step exists because the
   mistake has been made: one feature here was built and merged while an open
   pull request had been implementing the same thing since that morning, and the
   duplicate is the version that shipped. One listing is cheaper than
   rediscovering that.
1. Run `scripts/check.sh`. **If it fails, fix that first and do nothing else.**
   You did not break it, and leaving it broken makes the next session unable to
   tell their failure from yours.
2. Work on **only** your assigned feature. Do not refactor unrelated code, however
   tempting — an unrelated change in the same commit is indistinguishable from the
   feature when someone bisects.
3. **Write the test first**, then the code, until `scripts/check.sh` passes.
4. Commit with `git commit -S` — your agent signing key is configured; **never
   commit unsigned.** Message: `AG8-NNN: <what changed>` (the current Linear
   prefix is **`AG8-`**; the old `RCS-` prefix is retired).
5. **Open a pull request against `testnet`.** A PR is a proposal, not a merge —
   opening one never needs permission. Its branch name, title and body are public
   the instant they are pushed (see "This repository is public").
6. **Clear your context, then review your own diff cold** — a fresh adversarial
   pass, or the review skill. Reviewing in the same breath you wrote it defends
   your own bugs; a clean-context reviewer is the whole point.
7. **Fix every finding**, then re-run `scripts/check.sh`.
8. **Merge** only when CI is green **and** the review is clean: squash into
   `testnet`, set the ticket **Done**, delete the branch. **EXCEPTION — a
   sensitive path (below) or billing: PR and review as normal, but DO NOT merge.
   Leave it for Gus.**
9. Report back: ticket id, pass or fail, files changed, the PR link, and anything
   the next session must know.

## What you can do, and what needs Gus

- **Freely, no asking:** pick a ready `AG8-` ticket → branch → implement → run CI
  → open a PR → review it cold → fix findings → **self-merge to `testnet`** once
  CI is green and the review is clean.
- **Never self-merge — Gus merges:** anything touching a **sensitive path** (auth,
  crypto, `evm/`, wallet/key derivation, migrations, the invoice/payment/payout/
  refund handlers, `central-infrastructure`, or secrets/`.credentials`/env/RPC
  keys). Implement it, PR it, review it — a human merges it.
- **Never, under any circumstance:** merge to `main` or any mainnet deploy path;
  move real funds; force-push or rewrite shared history; delete, skip (`#[ignore]`)
  or weaken a test to make a check pass.
- **Ask first:** work you are not sure belongs in this *public* repo — a placement
  question (public repo vs the private tracker), not a per-PR gate.

## Rules that are not negotiable

**Never delete, skip (`#[ignore]`), or weaken a test to make a check pass.** If a
test blocks you, either the code is wrong or the test encodes a decision nobody
has revisited. Say which; do not edit it quietly. This rule survived three
attempts to make an exception for it in a single day, and each time the test was
right.

**A test that cannot fail is worse than no test.** Before trusting one, break the
thing it covers and confirm it goes red. Real examples from this repository: an
endpoint that returned 401 regardless of state; a duplicate-id case that failed
at the first statement so there was nothing to roll back; a guard whose own
self-test passed while the thing it guarded was being violated, because the test
used an untracked file the check never looks at.

**A unit test does not prove the feature is reachable.** Test it through the entry
point a caller actually uses. Three separate pieces of this repository shipped
fully tested and wired to nothing, every one with green tests. `pub` is not
reachability.

**State the method, not just the result.** "I verified X" is not useful without
what you ran and against what. A measurement against the wrong base, a query
with a limit that hid the rows, a log that was the wrong place to look — each of
those produced a confident wrong answer here within one day.

**Never `git add -A`.** Check `git status --short | grep '^??'` and add paths
explicitly. Build artefacts and symlinked `node_modules` have nearly been
committed that way.

## This repository is public

No ticket ids or tracker links in source — not in code, comments, doc comments
or test names. No session urls. No secrets, internal hostnames or private paths.
No reproduction for an unfixed vulnerability: describe the property now enforced
and leave the exploit in the private tracker.

**Some work belongs in the private repository rather than this one.** If you are
not certain a piece of work belongs here, ask before writing it — including
before naming a branch, writing a commit message, or titling a pull request, all
of which are public the moment they are pushed and cannot be retracted. The
private tracker says which work this applies to; this file deliberately does
not, because a rule that lists what it conceals discloses it.

Do not name any tool or vendor in a file, comment, or commit message.

## Conventions that bite

- **`git grep`, not bare `grep`.** `grep` here honours `.gitignore` and silently
  skips files. That has produced false "clean" results on leak scans.
- **`/tmp` is a RAM-backed tmpfs.** Never build there. A stale build directory
  filled it, and the consequences were a corrupted inter-process message, an
  unsignable commit, and a failed deploy that took the API down. Use `/var/tmp`.
- **Editing a migration changes its checksum** and breaks every deploy, including
  editing a comment. Renaming the file is free; changing its bytes is not.
- **Two migrations must never share a version number.** Branches opened the same
  day collide easily and each is green alone.
- **Commons is pinned by revision.** A change there does not reach this repo
  until the pin moves. Run `scripts/commons.sh unlink` before verifying, or a
  green build proves nothing about the pin.

## Sensitive paths

Auth, crypto, `evm/`, wallet and key derivation, migrations, and the invoice,
payment, payout and refund handlers. Work in them normally, but **say so
prominently in the commit message**, and **do not self-merge them** — these are
human-reviewed and human-merged without exception (the loop, step 8). Open the
PR, review it cold, fix findings, and leave the merge for Gus.

## What "done" means

`scripts/check.sh` exits zero **and** a test covers the feature. That script does
not run the integration suite without `DATABASE_URL`, and never runs end-to-end.
It prints what it skipped. A green run is not the same as mergeable, and
reporting it as such is the failure this file exists to prevent.
