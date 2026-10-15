# Receiving an account's standing

Status: slice 1 (the receiver) is built; later slices are design.

An external service decides whether each account is in good standing and
pushes that decision to this server. The receiver (slice 1 below) is
built: `POST /plugins/entitlements` answers, and `server/tests/account_standing_push.rs`
shows it refusing a stale version. The sender's delivery was held back until
exactly that was shown; that hold has since been lifted, and the sender no
longer has a withhold guard or a build feature that bypasses one. The later slices (reserved slug, read-only import,
filter) are not built here yet.

The push, as the sender emits it:

```
POST /plugins/entitlements
Authorization: Bearer <key>
{ "account_id": "<uuid>", "version": 6, "in_good_standing": true,
  "paid_through": "<rfc3339>" | null, "plan_name": "<text>",
  "checkout_url": "<url>" | null }
```

What the sender does with the answer: any 2xx is delivered and final; any
other status (and any network error) is stored as a failure and retried with
backoff (30 s doubling to 1 h); a push still failing after four hours is
logged at error on every attempt so a person sees it. It never reads the body.
Its `version` is per account and only rises, and rises only when the standing
changes.

## 1. Where the route lives, and who answers it

**A core route in the server, which stores the standing itself. A plugin only
reads it.**

- The route is `POST /plugins/entitlements`, mounted by the server beside the
  other `/plugins` routes. It does not collide with `/plugins/{id}/pages/...`
  (different shape), but `entitlements` must also be added to the reserved
  plugin slugs so no plugin can ever claim the name. That is a one-line
  change in `payserver-plugin-api`.
- Core owns it because the stored standing gates invoice creation. The
  property "a lower version never replaces a higher one" has to be proven
  against a real Postgres, and CI runs those tests; a property that lives
  inside a wasm module is far harder to prove and to ablate.
- A push must land even when the plugin is not loaded, is disabled, or is
  tripping its failure limit. If the plugin answered the route, an outage of
  the plugin would turn into 5xx to the sender, retries, and a finding raised
  about the wrong thing. It would also let malformed pushes count against the
  plugin's failure budget.
- The plugin must not be able to write the standing that gates it. Read-only
  access means a bug or compromise in the plugin cannot grant itself good
  standing.

Rejected: dispatching the route into a new plugin export, with the plugin
doing the compare-and-set in its own schema. It needs no host import, but the
receiver then cannot be tested without a built wasm module, and it couples
delivery to plugin health, as above.

The plugin reads the standing through **one new read-only host import**,
`account_standing(account_id) -> standing | none`: single account, no listing.

Deploy order, because a new import is load-bearing: a plugin that imports a
function the host does not provide fails to *instantiate*. So the order is
**server (route, table, import) first, plugin second**. Every earlier step is
harmless on its own: the route and table with no import just accumulate rows,
and an import nobody calls does nothing. Rolling the plugin out before the
server would stop the plugin from loading, and with it the invoice filter
(see section 5 for what the manifest does when that happens).

## 2. Authentication

The credential must be able to push standing and nothing else.

- **A second scope, separate from the merchant-listing scope.** Reading the
  list of merchants and writing the standing that gates their invoices are
  different powers; a key that can do the first must not be able to do the
  second. The new scope is a distinct entry in the key-permission set,
  validated at key creation the same way the listing scope is.
- A key carrying it is accepted **only** on this route, and it is refused
  everywhere else, including the routes an unscoped key reaches.
- Unlike the listing scope, **an unrestricted admin key is also refused** on
  this route. The route requires the scoped entry to be present. This is
  deliberate: the point is that the sender holds a credential that cannot be
  used for anything else, and accepting admin keys would let the sender be
  given one without anyone noticing.
- The key's owner must still be a server admin (a key never exceeds its
  owner, as with the listing scope), read from the owner's real role.
- The sender currently reuses one key for listing merchants and pushing, so it
  will need **two keys**, one per scope. Least privilege per call is worth a
  second secret.
- If the permission policy needs a constant that does not exist in
  `payserver-commons`, that is the usual three-step change (commons first, then
  the pin). The listing scope avoided it by reusing an existing policy; this
  one cannot honestly reuse one, because none means "push standing".

## 3. Storage and the compare-and-set

A new table keyed by account:

```
account_standing(
  account_id uuid primary key,
  version bigint not null check (version > 0),
  in_good_standing boolean not null,
  paid_through timestamptz null,
  plan_name text not null,
  checkout_url text null,
  received_at timestamptz not null default now()
)
```

This is a migration, and so sensitive. The apply is one statement:

```sql
INSERT INTO account_standing (account_id, version, in_good_standing,
                              paid_through, plan_name, checkout_url)
VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (account_id) DO UPDATE SET
    version = EXCLUDED.version,
    in_good_standing = EXCLUDED.in_good_standing,
    paid_through = EXCLUDED.paid_through,
    plan_name = EXCLUDED.plan_name,
    checkout_url = EXCLUDED.checkout_url,
    received_at = now()
WHERE EXCLUDED.version > account_standing.version
RETURNING version
```

Postgres takes the row lock for the conflicting row, so two concurrent pushes
for one account serialise and the loser is evaluated against the winner's
version; no read happens in application code. A row returned means applied; no
row means the held version is equal or higher, and nothing changed. To report
which, the handler then reads the held version (a read after the fact, used
only for the response body, never for the decision).

- **Same version again**: no row returned, nothing changed, answered as
  success. The body is not compared: the sender only raises a version when the
  content changed, so a same-version different-content push is a sender fault.
  It is logged at warn and the held value is kept.
- **Lower version**: no row returned, nothing changed, answered as success.
- Validation before the statement: `version >= 1`; `account_id` a UUID;
  `plan_name` non-empty and at most 200 bytes; `checkout_url`, if present, at
  most 2048 bytes and an `https` URL (it is rendered to merchants as a link,
  so a `javascript:` scheme must not be storable); body at most 4 KiB.
- The account is **not** required to exist. An existence check would be an
  oracle for valid account ids, and a standing that arrives before its account
  is harmless because only the invoice filter reads it, keyed by a real
  account.

## 4. What each answer means to the sender

| Situation | Status | Why |
| -- | -- | -- |
| Applied | 200, `{"applied":true,"version":n}` | delivered |
| Same version already held | 200, `{"applied":false,"version":held}` | already true; retrying changes nothing |
| Lower version than held | 200, `{"applied":false,"version":held}` | obsolete, not failed; a non-2xx here would make the sender retry a push that can never apply, then raise a finding after four hours |
| No `Authorization`, unknown or revoked key | 401 | must not be 2xx; the sender retries and surfaces it |
| Valid key without the push scope (including admin keys) | 403 | same |
| Malformed body, bad field, `version < 1` | 400 | the sender will never fix a malformed push by retrying, but the finding after four hours is the correct signal of a sender bug |
| Body too large | 413 | as above |
| Rate limited | 429 | transient; retry is right |
| Database unavailable | 503 | transient; retry is right |

Authentication is checked before the body is read, so an unauthenticated
caller learns nothing about validation. Only a stale or repeated version is
treated as success despite changing nothing.

## 5. How the invoice filter reads the standing

The filter keeps its manifest's fail-open or fail-closed choice for *failures*:
if it cannot run or the host call errors, the manifest decides, exactly as
today. This design does not change that. What it adds is what the filter does
when the call *succeeds*:

- **A row with `in_good_standing = true`**: allow.
- **A row with `in_good_standing = false`**: deny. The reason is the plugin's
  own words, naming the standing and offering `checkout_url`, because the
  merchant has nowhere else to learn why.
- **No row**: allow.

No row means this server has never been told anything about the account. That
must allow: the sender only pushes when standing *changes*, so an account that
existed before the first push has no row and has no reason to get one. Denying
on "no row" would refuse every existing merchant's invoices the moment the
filter starts reading. The cost is real and accepted: an account that lapsed
while its push never arrived keeps creating invoices. That is bounded by the
sender's own alarm (four hours of failure), and by the initial full push in
the slices below, which gives every account a row.

`paid_through` is for display. The filter decides on `in_good_standing` alone:
the sender is the one place that decides standing, and a second, clock-based
decision here would lock accounts out whenever a push is late.

The filter only ever refuses a not-yet-created invoice. Nothing in this design
touches a payment already sent.

## 6. Slices

In order; each is one PR. **S** marks sensitive (human review first).

1. **Receiver (S: auth, migration, gates invoice creation).** The scope,
   the migration, the compare-and-set in the data layer, the route with the
   answers in section 4, the OpenAPI entry. Acceptance: through the real
   router and a real Postgres, send v6 then v5, assert v6 is held; replace the
   compare with an unconditional update and the test goes red. Also a test
   per row of the table in section 4, including that an unrestricted admin
   key gets 403, and a concurrency test (two pushes raced, higher always
   wins).
2. **Reserve the slug (commons).** Add `entitlements` to the reserved
   slugs. Can land alongside the next commons change; no behaviour change.
3. **Read-only import (commons, then S: host call here).** Commons adds
   `account_standing`; merge, then bump the pin here and implement it on the
   plugin host-call set. Instantiation test on a module importing only it.
4. **Filter reads the standing (billing, private).** Ships only after 3 is
   deployed, per section 1.
5. **Sender (billing, private).** Use the second key; push every account once
   (initial full push). The sender's hold on delivery, which waited on
   slice 1's acceptance test, has been lifted. Slice 1 is done; this step
   belongs to the sender.

Mainnet gets each of these only through a release tag, after testnet has run
the full sequence, because mainnet holds real merchant funds and there is no
staging.
