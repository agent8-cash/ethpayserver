//! Recording a `devices` row for an already-authenticated account.
//!
//! SENSITIVE: mints a new standing login credential (a `devices` row) tied
//! to a symmetric-key blob the client supplies. Human review required
//! without exception.
//!
//! Deliberately narrow, and deliberately not touching WebAuthn at all.
//! Registering a second passkey is already handled by
//! `POST /auth/passkey/register/start` and `/complete`, mounted unchanged
//! from the `auth` crate; this handler does not duplicate that.
//!
//! What was missing is the step after it: no code path recorded a `devices`
//! row for an already-logged-in user's new passkey, or enforced the device
//! cap for it (`complete_passkey_registration` only checks the passkey
//! cap). Today that row is only ever minted lazily, the next time the new
//! passkey is used to log in (`device_for_new_login`/`reusable_device` in
//! the `auth` crate), which leaves a freshly-added passkey unable to
//! decrypt anything until that happens. This endpoint lets the client
//! supply the row, and the device cap it should have been checked against,
//! at add-device time instead.
//!
//! The server never sees a plaintext key here, only the opaque
//! `EncryptedBlob`/`KdfParams` the client already derived with the same
//! KDF/AEAD primitives registration uses (see the `crypto` crate); this
//! handler stores them as-is and never attempts to decode what they contain.
//!
//! Kept in this repository rather than in the `auth` crate so it can use the
//! richer session extractors already defined here - the same boundary
//! reasoning `users/wallets.rs` documents for wallet-signature verification.
//! Gated on `FreshlyAuthenticatedUser`, not merely a valid session: adding a
//! standing credential is at least as sensitive as the account's recovery
//! email, which is the existing precedent for requiring a just-completed
//! login rather than a merely-valid one.

use auth::{AuthConfig, Device, DeviceInfo, DeviceRepository, DeviceType, SessionService};
use axum::{Json, extract::State, http::StatusCode};
use crypto::{EncryptedBlob, KdfParams};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::api::extractors::FreshlyAuthenticatedUser;
use crate::state::PgAppState;

/// Body for `POST /users/me/devices`.
///
/// `encrypted_symmetric_key`/`kdf_params` arrive pre-wrapped by the client;
/// this handler stores them opaquely.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddDeviceRequest {
    pub device_name: String,
    pub device_type: DeviceType,
    /// Opaque to the server - base64-encoded ciphertext/iv/mac fields, never
    /// decoded here.
    #[schema(value_type = Object)]
    pub encrypted_symmetric_key: EncryptedBlob,
    #[schema(value_type = Object)]
    pub kdf_params: KdfParams,
}

/// Record a `devices` row for the caller's account, enforcing the same
/// device cap login-time device creation already enforces
/// (`AuthConfig::default().max_devices_per_user`).
///
/// Pairs with `POST /auth/passkey/register/{start,complete}` for the
/// WebAuthn half of "add a device" - call this right after that pair
/// succeeds so the new device is usable immediately, with the cap checked
/// before the row is created rather than after.
#[utoipa::path(
    post,
    path = "/users/me/devices",
    tag = "users",
    security(("bearer_auth" = [])),
    request_body = AddDeviceRequest,
    responses(
        (status = 200, description = "Device recorded", body = DeviceInfo),
        (status = 401, description = "No session, or not a recently-completed login"),
        (status = 409, description = "Device cap reached"),
    )
)]
pub async fn add_device<A>(
    FreshlyAuthenticatedUser(user): FreshlyAuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(req): Json<AddDeviceRequest>,
) -> Result<Json<DeviceInfo>, StatusCode>
where
    A: SessionService + 'static,
{
    // ponytail: count-then-insert, not a transaction, so two concurrent
    // enrollments can both pass this check and land one over the cap - the
    // same gap `complete_new_user_passkey_registration` already has across
    // its own four inserts (user/passkey/device/session), unexamined until
    // now. Upgrade path: wrap the count and the insert in one
    // `SELECT ... FOR UPDATE`-guarded transaction if the cap needs to be
    // exact rather than approximate.
    let active = state
        .data_service
        .count_active_devices(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // The same cap `complete_passkey_login` enforces at login time
    // (`auth::service::device::device_for_new_login`) - reused, not
    // reinvented, so the two paths cannot silently disagree on the limit.
    if active >= AuthConfig::default().max_devices_per_user {
        return Err(StatusCode::CONFLICT);
    }

    let device = Device::new(
        user.id,
        req.device_name,
        req.device_type,
        req.encrypted_symmetric_key,
        req.kdf_params,
    );
    let info = DeviceInfo::from(&device);

    state
        .data_service
        .create_device(&device)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(info))
}
