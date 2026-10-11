# Rules for a coding session

You are one session working on one feature. Read this, then
`memos/spec.md`, `git log --oneline -20`, and the ticket you were given. There is
no session log: the tickets and the commit history carry that context.

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
4. Commit with `git commit -S`. Message: `<ticket id>: <what changed>`.
5. Report back: ticket id, pass or fail, files changed, and anything the next
   session must know.

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
prominently in the commit message**. These are human-reviewed without exception.

## What "done" means

`scripts/check.sh` exits zero **and** a test covers the feature. That script does
not run the integration suite without `DATABASE_URL`, and never runs end-to-end.
It prints what it skipped. A green run is not the same as mergeable, and
reporting it as such is the failure this file exists to prevent.
