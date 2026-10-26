## Device ID Audit

### Section 1: Context
At the device cap, the least-recently-used active device is re-used instead of returning `MaxDevicesReached`. This overwrites the record's `name`, `device_type`, `encrypted_symmetric_key`, `kdf_params`, and `last_used_at`, and returns the same `DeviceId`. This allows one `device_id` to back sessions on multiple machines. Audited the commons at `665e343` or later, and ethpayserver at `db03bd413e847ee5465d002b0536995c1da247ef`.

### Section 2: Inventory Table
| Site | Read/Write | Assumption it makes | Trust? |
| --- | --- | --- | --- |
| **payserver-commons, `auth`** | | | |
| `auth/src/models.rs` `Device` struct (lines 313-343), `id` | Read | It is the record key. `is_active` is documented "active/trusted", but the doc comment is the only place "trusted" appears. No code grants anything on it. | No |
| `auth/src/models.rs` `Session.device_id` (line 396) | Write | It is an informational back-pointer from session to device record. `validate_session` (lines 29-63) never reads it. | No |
| `auth/src/models.rs` login request `device_id: Option<DeviceId>` (around lines 919-921 and 1117-1118) | Read | It is a client-supplied hint about which record to attach to. | No |
| `service/device.rs` `reusable_device` (lines 81-104) | Read | The hint is accepted only if the device exists, belongs to the same user and is active. Otherwise it is treated as "no hint". It runs after the credential is verified, so it never skips verification. | No |
| `service/wallet.rs` login (lines 280-296, 305, 314) and `service/passkey.rs` login (around lines 295-330) | Read then write | They call `reusable_device`, then `update_device` with a new `last_used_at`, then create the session. They assume the id only picks a record. | No |
| `service/wallet.rs` registration (around lines 435-455), `service/passkey.rs` registration (around lines 150-170) and `service/recovery.rs` (lines 279-300) | Write | Each always mints a fresh device via `Device::new`/`device_for_new_login`. None accepts a client id. | No |
| `service/device.rs` `device_for_new_login` (lines 117-162) | Write | It is the new LRU reuse. It picks by `last_used_at.unwrap_or(created_at)`. | No |
| `service/device.rs` `revoke_device` (lines 42-66) | Read and write | It refuses to revoke the caller's own device. It checks ownership, then calls `delete_sessions_for_device`, then `deactivate_device`. **Behavioural consequence, not trust:** revoking a shared id logs out every machine attached to it. The device list can no longer tell the user which machine they are revoking. | No |
| `service/device.rs` `get_devices` (lines 25-36) | Read | It lists active devices as `DeviceInfo`, with no key material. The `name` and `type` shown are those of the last login, because LRU reuse overwrites them. | No |
| `repository.rs` trait methods: `get_device` (line 75), `deactivate_device` (84), `delete_device` (87), `delete_sessions_for_device` (115), plus the in-memory test impls (lines 459-800). Storage only. | No |
| `api/management.rs` `DELETE /auth/devices/{id}` (lines 103-117) and `traits.rs` line 194. Pass-through to `revoke_device`. | No |
| `ui-kit/src/auth/session.rs` `DEVICE_ID_KEY = "ps_device_id"` (line 11), written at line 19 and read by `get_device_id` (line 45). `login_page.rs` lines 98-103 and 163-167 send it back on login. It is per-browser localStorage and is never cleared on rejection. Two browsers sharing one id after reclaim is harmless. A cleared storage just mints or reclaims a device. | No |
| `ui-kit/src/auth/types.rs` lines 31, 108, 163, 257, 299, 310. Plain DTO fields. | No |
| **ethpayserver:** | | | |
| `data-service/migrations/postgres/20241214000001_create_auth_tables.sql` line 87 (`sessions.device_id` is `NOT NULL REFERENCES devices(id) ON DELETE CASCADE`) and line 101 (`idx_sessions_device_id`). Deleting a device deletes its sessions. This is referential integrity only. | No |
| `data-service/src/postgres/auth/session.rs` lines 20-27 (insert), 43 and 136-154 (select/map) and 96-98 (`delete_sessions_for_device`). Storage only. | No |
| `data-service/src/postgres/auth/device.rs` lines 15-150. It implements `DeviceRepository`. `update_device` (lines 77-106) is what LRU reuse calls. It does not write `user_id`, so the record cannot change owner. `count_active_devices` counts `is_active` rows for the cap. | No |
| `server/src/api/plugins.rs` lines 388-424, `server/src/api/refunds/tests.rs` lines 22 and 62, `server/tests/ws_store_scope.rs` line 70 and `server/tests/standing_warning_header.rs` lines 72-90. Test fixtures only. They build a `Session` with a throwaway `DeviceId`. | No |
| `e2e/fixtures/synthetic-account.ts` line 60 and `e2e/tests/ws-tenant-isolation.spec.ts` line 118 send only `device_name`/`device_type`, never a `device_id`. `e2e/fixtures/db.ts` line 43 truncates `devices`. | No |
| `mcp-server/src/testkit/auth_repo.rs` line 137. Test repo, `last_used_at: None`. | No |
| Negative finding: no non-test code in `server/`, `evm/`, `mcp-server/` or `data-service/src` reads `session.device_id` or `DeviceId`. Auth middleware gets a `Session` back and uses only user id and role. The coder must confirm this by running `git grep -n -i "device_id\|DeviceId" -- server evm mcp-server data-service/src` and listing any hit not covered above. | No |

### Section 3: Trust-bearing Mechanisms Checked and Not Found
- No remembered-device or "skip verification on known device" logic exists. `reusable_device` runs only after the credential is verified.
- No 2FA/TOTP code.
- No refresh tokens. Sessions are opaque `SessionId`s with absolute expiry plus an optional idle timeout. They are not bound to a device or to an IP or user agent. IP and user agent are audit-only.
- The wallet re-auth challenge (`data-service/src/postgres/auth/wallet.rs`, lines 254-306) is keyed by `user_id`, not by device.
- Rate limits and API keys are not keyed by device.

### Section 4: Non-trust Consequences to Hand to Chefagent
1. Revoke blast radius (row 8): revoking a shared record kills every attached machine's sessions.
2. Display staleness (row 9): the device list shows the last login's name and type.
3. The reclaimed record has its `encrypted_symmetric_key` and `kdf_params` overwritten from the user's current values. This is correct as long as the device blob is a copy of the user's key and nothing per-device. State that this was observed and not exercised.
4. Reclaim does not invalidate the previous holder's live session. Both stay valid until expiry or revoke. No data is exposed, because the session belongs to the same user.
5. `reusable_device` updates `last_used_at` (in the wallet login path) but does not rename. Only reclaim renames.

### Section 5: Verdict
**Verdict: no consumer of device_id in payserver-commons or ethpayserver carries trust on it. The rev move is not blocked by this audit.**
The commons `rev` is not bumped by this change, and the move is a separate decision.

## Verification
Run `git grep -n -i "device_id\|DeviceId"` in both repos as a final check that the table omits no site. Use `git grep`, never plain `grep`. Stage only `docs/device-id-audit.md`, never `git add -A`. No Rust changes, so fmt, clippy and nextest are unaffected.
=======
the complete file content
