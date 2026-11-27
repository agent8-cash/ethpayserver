//! Wallet login credentials.
//!
//! SENSITIVE: this section lists and changes login-credential wallets and the
//! primary-wallet pointer wallet login resolves accounts by. Human review
//! required without exception.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use sha3::Digest;
use uuid::Uuid;

use auth::{SessionService, WalletCredential, WalletCredentialId, WalletRepository};

use super::key_material::generate_key_segment;
use crate::api::extractors::AuthenticatedUser;
use crate::state::PgAppState;

/// A wallet credential, as returned to the account owner.
///
/// Hand-mirrors `auth::WalletInfo` rather than reusing it directly: `auth` is
/// a server-side crate the client does not depend on (see
/// `api_keys::api_key_info_response` for the same reasoning), and this DTO
/// can't be added to the shared `api-types` crate in this change — that crate
/// is pinned by `rev` in a sibling repository and moving the pin is its own
/// three-step change. The client defines a matching struct for deserializing
/// this response.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct WalletCredentialResponse {
    pub id: Uuid,
    pub address: String,
    pub name: String,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl From<WalletCredential> for WalletCredentialResponse {
    fn from(w: WalletCredential) -> Self {
        Self {
            id: w.id.0,
            address: w.address,
            name: w.name,
            is_primary: w.is_primary,
            created_at: w.created_at,
            last_used_at: w.last_used_at,
        }
    }
}

/// List the authenticated account's wallet login credentials.
///
/// A plain valid session is enough to *read* this — it is the same
/// information `GET /auth/wallets` already returns. Only the write below
/// (making one of them primary) is gated on a fresh re-authentication.
#[utoipa::path(
    get,
    path = "/users/wallets",
    tag = "users",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Wallet credentials for this account", body = Vec<WalletCredentialResponse>),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn list_wallet_credentials<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Result<Json<Vec<WalletCredentialResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    let mut wallets = state
        .data_service
        .get_wallets_for_user(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    wallets.retain(|w| w.is_active);

    Ok(Json(
        wallets
            .into_iter()
            .map(WalletCredentialResponse::from)
            .collect(),
    ))
}

/// How long a wallet-reauth challenge stays answerable. Generous enough to
/// unlock a wallet extension and approve its signing prompt, short enough
/// that an unused challenge does not linger as a standing credential. Also
/// enforced in SQL by `take_wallet_reauth_challenge`, which is what the
/// endpoints below actually rely on — this constant only has to agree with
/// that query, not re-derive it.
const WALLET_REAUTH_CHALLENGE_TTL_SECS: i64 = 5 * 60;

/// Build the message a wallet extension shows in its signing prompt for a
/// reauth challenge.
///
/// Deliberately worded differently from the auth crate's own login-challenge
/// message ("Sign this message to authenticate to..."): the two ceremonies
/// use separate challenge storage (`wallet_reauth_challenges` vs.
/// `wallet_challenges`) and must never be interchangeable, so the text a
/// merchant is asked to sign should look different too, not just hash
/// differently.
fn wallet_reauth_challenge_message(
    challenge: &str,
    address: &str,
    created_at: DateTime<Utc>,
) -> String {
    format!(
        "Confirm this wallet change on agent8.cash:\n\nChallenge: {challenge}\nTimestamp: {}\nAddress: {address}",
        created_at.to_rfc3339()
    )
}

/// Verify an EIP-191 `personal_sign` signature recovers to `expected_address`.
///
/// Hand-mirrors `auth::service::wallet::verify_wallet_signature`: that
/// function (and the EIP-191/Keccak256/ECDSA-recovery it does) lives
/// `pub(super)` inside the pinned `auth` crate in payserver-commons, not
/// reachable from here, and making it public is its own three-step commons
/// change (merge there, bump the pinned rev, `cargo update`) this ticket
/// does not need — the challenge this checks is entirely local to
/// `wallet_reauth_challenges` and never mixes with `auth`'s own login
/// challenges. Same recovery arithmetic, independent code path.
fn verify_wallet_signature(message: &str, signature_hex: &str, expected_address: &str) -> bool {
    let Ok(signature_bytes) = hex::decode(signature_hex.trim_start_matches("0x")) else {
        return false;
    };
    if signature_bytes.len() != 65 {
        return false;
    }
    let (r_s, v) = signature_bytes.split_at(64);
    let Ok(signature) = k256::ecdsa::Signature::from_slice(r_s) else {
        return false;
    };
    let recovery_id = match v[0] {
        27 | 0 => k256::ecdsa::RecoveryId::try_from(0u8),
        28 | 1 => k256::ecdsa::RecoveryId::try_from(1u8),
        _ => return false,
    };
    let Ok(recovery_id) = recovery_id else {
        return false;
    };

    let prefix = format!("\x19Ethereum Signed Message:\n{}", message.len());
    let mut hasher = sha3::Keccak256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(message.as_bytes());
    let message_hash = hasher.finalize();

    let Ok(recovered_key) =
        k256::ecdsa::VerifyingKey::recover_from_prehash(&message_hash, &signature, recovery_id)
    else {
        return false;
    };

    let public_key_bytes = recovered_key.to_encoded_point(false);
    let public_key_hash = sha3::Keccak256::digest(&public_key_bytes.as_bytes()[1..]);
    let recovered_address = format!("0x{}", hex::encode(&public_key_hash[12..]));

    recovered_address.eq_ignore_ascii_case(expected_address)
}

/// What the caller must sign to prove they currently hold the private key
/// for the wallet credential they are about to promote.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct WalletReauthChallengeResponse {
    pub message: String,
    pub expires_in_secs: i64,
}

/// Request a proof-of-possession challenge for wallet credential `id`.
///
/// Step one of promoting a wallet to primary. A session alone — even a
/// freshly-minted one — proves who is logged in, not that the caller still
/// controls the address being promoted to a login credential: a hijacked
/// session has no notion of "just logged in" strong enough to rule that out
/// on its own. This challenge, and the signature `PATCH .../primary` below
/// requires against it, ask for the one thing a session hijack cannot
/// forge — a fresh signature from the wallet's own key.
#[utoipa::path(
    post,
    path = "/users/wallets/{id}/reauth-challenge",
    tag = "users",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Wallet credential to prove current ownership of")),
    responses(
        (status = 200, description = "Challenge issued", body = WalletReauthChallengeResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such active wallet credential on this account"),
    )
)]
pub async fn create_wallet_reauth_challenge<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(id): Path<Uuid>,
) -> Result<Json<WalletReauthChallengeResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let wallet = state
        .data_service
        .get_wallet(WalletCredentialId(id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|w| w.user_id == user.id && w.is_active)
        .ok_or(StatusCode::NOT_FOUND)?;

    let challenge = generate_key_segment(32);
    let created_at = Utc::now();

    state
        .data_service
        .store_wallet_reauth_challenge(user.id, &wallet.address, &challenge, created_at)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(WalletReauthChallengeResponse {
        message: wallet_reauth_challenge_message(&challenge, &wallet.address, created_at),
        expires_in_secs: WALLET_REAUTH_CHALLENGE_TTL_SECS,
    }))
}

/// Body for `PATCH /users/wallets/{id}/primary`: the signature over the
/// challenge issued by `POST /users/wallets/{id}/reauth-challenge`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct PromoteWalletCredentialRequest {
    pub signature: String,
}

/// Make an existing wallet credential the account's primary — the address
/// wallet login resolves the account by, and the one shown in Settings.
///
/// Requires both a valid session (`AuthenticatedUser`) and a signature over
/// a fresh `POST .../reauth-challenge` issued for this exact wallet — proof
/// that the caller controls the address right now, not just that they proved
/// it once at `complete_wallet_registration` time and are currently carrying
/// a session cookie. The two checks cover different attackers: the session
/// rules out an anonymous caller, the signature rules out a hijacked session
/// that never held the wallet's key.
///
/// Deliberately does not accept a bare address plus signature for a brand
/// new address. `id` must already name an active `WalletCredential`
/// belonging to this account — ownership of that address was already proven
/// once, via the existing challenge/signature flow, when it was added
/// (`complete_wallet_registration`) or at account creation. Accepting an
/// unregistered address here instead would let this endpoint be used to
/// register a credential outside that flow.
#[utoipa::path(
    patch,
    path = "/users/wallets/{id}/primary",
    tag = "users",
    security(("bearer_auth" = [])),
    params(("id" = Uuid, Path, description = "Wallet credential to make primary")),
    request_body = PromoteWalletCredentialRequest,
    responses(
        (status = 200, description = "Primary wallet changed", body = WalletCredentialResponse),
        (status = 401, description = "Unauthorized, or no valid reauth signature for this wallet"),
        (status = 404, description = "No such active wallet credential on this account"),
    )
)]
pub async fn set_primary_wallet_credential<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<PromoteWalletCredentialRequest>,
) -> Result<Json<WalletCredentialResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let wallet = state
        .data_service
        .get_wallet(WalletCredentialId(id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|w| w.user_id == user.id && w.is_active)
        .ok_or(StatusCode::NOT_FOUND)?;

    let challenge = state
        .data_service
        .take_wallet_reauth_challenge(user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // The challenge must have been issued for exactly this wallet's address —
    // a challenge answered for a different credential must not authorize
    // promoting this one.
    if !challenge.address.eq_ignore_ascii_case(&wallet.address) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let expected_message = wallet_reauth_challenge_message(
        &challenge.challenge,
        &challenge.address,
        challenge.created_at,
    );
    if !verify_wallet_signature(&expected_message, &payload.signature, &challenge.address) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    state
        .data_service
        .set_primary_wallet_credential(user.id, WalletCredentialId(id))
        .await
        .map(|w| Json(WalletCredentialResponse::from(w)))
        .map_err(|e| match e {
            auth::AuthError::WalletNotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })
}

#[cfg(test)]
#[path = "wallets_tests.rs"]
mod tests;
