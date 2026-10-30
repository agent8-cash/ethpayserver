# `device_id` consumer audit (AG8-807)

## Why

RCS-737 (payserver-commons `665e343`) changed what happens at the per-user
device cap: a login that used to be refused now reclaims the least-recently-used
active device instead. Two physical machines can now carry the same
`device_id` over time (one machine's login reclaims the record another
machine most recently held). The fix assumes `device_id` carries no identity
guarantee worth relying on — this audit checks whether anything in
`payserver-commons` or `ethpayserver` actually relies on it anyway. The
`ethpayserver` Cargo pin on `payserver-commons` is **not** moved by this
ticket; that is a separate decision this doc exists to inform.

## Method

`git grep -n device_id` across both repositories, every call site opened and
read in full context (not inferred from the variable name). Also checked for
device-scoped trust primitives that might not have `device_id` in the name
directly: 2FA/MFA/TOTP, "remembered device", refresh tokens, re-auth skip.
Neither repository implements MFA/TOTP today; the only re-auth primitive
(`wallet_reauth_challenges`) is keyed by `user_id`, not `device_id` — checked
explicitly below.

## payserver-commons

### `auth/src/service/device.rs`

- **L42-66 `revoke_device`** — before deleting sessions/deactivating a
  device, checks `session.device_id == device_id` to refuse revoking the
  device backing the *caller's own current session*, and separately checks
  `device.user_id == user_info.id` (ownership). Both checks run only after
  `validate_session` has already authenticated the caller via the session
  id. `device_id` here is not itself a credential — it decides *which*
  already-authenticated action is allowed (can't revoke yourself), not
  *whether* the caller is authenticated. Opaque label/foreign key use.
- **L81-104 `reusable_device`** — looks up a client-sent `device_id`;
  returns the device only if it exists, belongs to the calling user, and is
  active, otherwise returns `None` and logs (stale/unknown id treated as "no
  hint"). Called only from `complete_passkey_login` and
  `complete_wallet_login`, **after** the passkey assertion / wallet signature
  has already been cryptographically verified (see below). The id is
  explicitly documented as "only a hint about which device record to attach
  the session to" — never consulted as a reason to grant or skip
  authentication. Opaque label.
- **L117-162 `device_for_new_login`** — this is the RCS-737 change itself.
  Below `max_devices_per_user` it creates a new device row; at the cap it
  reclaims the least-recently-used active device (new name/type/key
  material, `last_used_at` refreshed) rather than refusing. No trust
  decision reads `device_id` here; it mutates device *records*, called after
  credential verification, same as above. Opaque label / bookkeeping.

### `auth/src/service/passkey.rs`

- **L152-166 login (new-user passkey path)** —
  `device_id = device.id` taken from the freshly created device, stamped
  into the new `Session`. Bookkeeping only.
- **L260-311 `complete_passkey_login`** — WebAuthn assertion verified first
  (L260-270, failure path returns `PasskeyVerificationFailed` and does not
  touch `device_id` at all); only *after* that success does it call
  `reusable_device(user.id, request.device_id)` (L300) and attach the
  resulting device id to the new `Session` (L320). `device_id` plays no role
  in whether the login succeeds — it is resolved strictly after the
  authentication decision is already made. Opaque label.

### `auth/src/service/recovery.rs`

- **L279-298** — account recovery (`complete_account_recovery`, password/key
  recovery flow) **always creates a brand-new `Device`** — it does not read
  any client-supplied `device_id` at all; `CompleteRecoveryRequest` (see
  `models.rs`) has no `device_id` field. Recovery revokes every existing
  device and session for the user (L260-264) before minting the one new
  device. `device_id` is not an input to this flow in any form — not trusted,
  not consulted.

### `auth/src/service/wallet.rs`

- **L250-321 `complete_wallet_login`** — signature verified first (L253-267;
  on `WalletSignatureVerificationFailed` it increments failed-login/lockout
  counters and returns *before* `device_id` is ever touched); only on
  success does it call `reusable_device(user.id, request.device_id)` (L285)
  and attach the result to the new `Session` (L305). Same shape as the
  passkey path: `device_id` resolved strictly after the auth decision.
  Opaque label.
- **L387-459 `complete_new_user_wallet_registration`** — brand-new user
  flow; unconditionally creates a fresh `Device` (L431-439), ignoring any
  client-side `device_id` (the request type carries none). Opaque label,
  trivially — nothing to trust yet.

### `auth/src/models.rs`

- **L396, 416-440 `Session` struct / constructors** — `device_id: DeviceId`
  is a plain field copied from whatever `device_for_new_login`/
  `reusable_device` resolved. No logic branches on it beyond storage.
- **L541 (`DeviceInfo`), L920-921 / L1105-1118 (`CompletePasskeyLoginRequest`,
  `CompleteWalletLoginRequest`)** — doc comments on these fields ("Device ID
  from a previous login on this device... server will reuse the existing
  device record") describe exactly the hint semantics above: client-supplied,
  optional, used only to pick a device record to reuse, never to grant
  anything. No code here contradicts that comment.

### `auth/src/repository.rs`

- **L115, L794-797 `delete_sessions_for_device`** (trait + in-memory test
  impl) — deletes session rows filtered by `device_id`. Storage/filter key
  only.

### `auth/src/service/mod.rs` (L160-161) and `auth/src/traits.rs` (L194)

- Trait delegation / signature declarations for `revoke_device`. No
  independent logic; covered by `device.rs` above.

### `auth/src/api/management.rs` (L110-121)

- HTTP handler for `DELETE /auth/devices/{id}`. Takes `device_id` from the
  URL path, requires an `AuthenticatedRequest` (session id) in the body,
  passes both straight to `revoke_device`. Authentication is the session id,
  not the path's `device_id`; same shape as the service-layer check above.

### `ui-kit/src/auth/session.rs` and `ui-kit/src/auth/components/login_page.rs`

- Client (WASM) side. `get_device_id()` reads `ps_device_id` from
  `localStorage` (persisted independently of the session, L50-53: survives
  `clear_session()`, only `clear_all_auth_data()` removes it) and the login
  pages (`login_page.rs` L98, L163) send it as `device_id` on
  `CompleteWalletLoginRequest`/`CompletePasskeyLoginRequest`. This is the
  value `reusable_device` treats as a hint server-side. The client never
  uses the stored `device_id` itself to skip, shortcut, or pre-authorize
  anything — it is sent alongside the credential (signature / WebAuthn
  assertion), never instead of one. Opaque label, client side.
- `ui-kit/src/auth/types.rs` — `StoredSession`/`LoginResponse`/request type
  fields carrying `device_id` end to end. Pure plumbing, no logic.

### Device-scoped trust primitives checked and ruled out

- No MFA/TOTP exists anywhere in `payserver-commons` today (`grep` for
  `2fa`/`mfa`/`totp` found nothing in `auth/` or `ui-kit/`), so there is no
  "remembered device skips 2FA" path to audit — the mechanism doesn't exist,
  trusted or not.
- `wallet_reauth_challenges` (the one re-auth primitive in the data layer,
  `data-service/migrations/postgres/20260914000001_wallet_reauth_challenges.sql`
  and `store_wallet_reauth_challenge`/`take_wallet_reauth_challenge`) is keyed
  by `user_id`, never `device_id`. Confirmed by reading the migration and the
  integration tests (`data-service/src/postgres/auth/integration_tests.rs`
  L510-553) — no `device_id` column or parameter anywhere in that path.
- No refresh-token concept exists in this auth system — sessions are
  looked up by `SessionId` directly (see `ethpayserver` below); there is no
  token that `device_id` could be bound to.

## ethpayserver

Every site here is the persistence layer underneath `payserver-commons`'
`SessionRepository`/`DeviceRepository` traits, plus test fixtures. None of it
makes an authorization decision; `device_id` is a stored/queried column.

- **`data-service/migrations/postgres/20241214000001_create_auth_tables.sql`
  L84-103** — `sessions.device_id` is a `NOT NULL` FK into `devices(id)` with
  an index. Schema only; referential integrity, not trust.
- **`data-service/src/postgres/auth/session.rs`** — `SessionRepository` impl:
  `create_session`/`get_session`/`get_sessions_for_user` bind/read
  `device_id` as a plain column (L20-33, L43-54, L136-146);
  `delete_sessions_for_device` (L96-103) deletes by that column. No branch
  anywhere reads `device_id` to decide whether to authenticate or authorize
  anything — every one of these functions is reached only after
  `payserver-commons` has already resolved a session/device id to act on.
- **`data-service/src/postgres/auth/tests.rs`, `integration_tests.rs`** —
  test fixtures/assertions (`test_session`, `assert_eq!(fetched.device_id,
  device.id)`, etc.). Confirms round-tripping, not a behavior.
- **`server/src/api/plugins.rs` L415-424** — a unit-test fake
  (`FakeSessions::validate_session`) stamps a hardcoded `DeviceId` into a
  `Session` purely so the test fixture type-checks. Not production code path.
- **`server/tests/standing_warning_header.rs` L72-93** — integration-test
  seed data: inserts a row into `devices` then a `sessions` row referencing
  it by FK, so the test's session satisfies the schema. Fixture only.

## Verdict

**No consumer in `payserver-commons` or `ethpayserver` carries trust on a
`device_id`.**

Every write of `device_id` happens after an independent authentication
decision has already been made (a verified WebAuthn assertion, a verified
wallet signature, or an already-validated session id) — never before, and
never as a substitute for one. Every read of `device_id` either (a) picks
which device *record* to update/reuse/revoke, a bookkeeping choice gated by
an ownership check (`device.user_id == user_info.id`) and an activity check
(`is_active`), or (b) is a plain storage column (foreign key, index,
`SELECT`/`DELETE ... WHERE device_id = $1`). There is no remembered-device
2FA/MFA skip (the mechanism does not exist in this codebase), no refresh-token
or session binding keyed by device identity (sessions are looked up by
`SessionId`, not by `device_id`), and no re-auth skip tied to device identity
(`wallet_reauth_challenges` is keyed by `user_id`). RCS-737's premise — that a
`device_id` is a storage hint, not a machine identity, and that two machines
sharing one is therefore safe to allow at the cap — holds on both sides of
the pin.

This clears the premise the rev-move decision depends on. The rev move
itself is a separate decision and is **not** made by this document or this
PR; `ethpayserver`'s `payserver-commons` pin is unchanged here.
