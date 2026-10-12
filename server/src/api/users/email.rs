//! Email change: request, confirm, remove.
//!
//! SENSITIVE: this touches account recovery. Set, change and remove all sit
//! behind `FreshlyAuthenticatedUser` (server/src/api/extractors.rs) rather than
//! `AuthenticatedUser` - a session hijacked hours into its lifetime must not be
//! able to swap the account's recovery address. None of this touches
//! `kdf_salt_identifier`: it is set once at registration and is immutable
//! (enforced in `auth::UserRepository::update_user`), and the recovery KDF
//! stays salted with whatever identifier was pinned then regardless of what
//! `email` becomes later. That is what makes changing email safe at all.

use axum::{Json, extract::State, http::StatusCode};
use chrono::Utc;
use uuid::Uuid;

use auth::SessionService;

use crate::api::extractors::{AuthenticatedUser, FreshlyAuthenticatedUser};
use crate::services::EmailChangeVerificationData;
use crate::state::PgAppState;

/// How long a pending email change's verification code remains redeemable.
const EMAIL_CHANGE_TOKEN_TTL: chrono::Duration = chrono::Duration::minutes(30);

/// Body for `POST /users/me/email`. Covers both setting an email where there
/// was none and changing an existing one - the server treats them identically,
/// since either way the new address must be verified before it lands.
///
/// Defined here rather than in `api-types`: that crate is where a JSON body
/// normally belongs so the client shares the exact shape instead of a
/// hand-mirrored copy (see its module doc - a drifted copy has broken the
/// client on contact with the API before). But `api-types` lives in
/// `payserver-commons`, and landing a change there is the three-step dance
/// this repo's `CLAUDE.md` describes: merge in commons, bump the pinned `rev`
/// here, only then does the code see it - a cross-repo review this single
/// ticket cannot complete on its own. `DeleteAccountQuery` took the same
/// exception for the same reason. The shape is two primitive fields, so the
/// drift risk is small and worth accepting rather than blocking this ticket
/// on an external merge.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct RequestEmailChangePayload {
    pub new_email: String,
}

/// Body for `POST /users/me/email/confirm`. See `RequestEmailChangePayload`
/// for why this is local rather than in `api-types`.
///
/// Deliberately unauthenticated: the token itself, delivered only to the
/// address being verified, is the proof. Requiring a session on top would add
/// nothing but a way for a merchant who requested the change from one browser
/// to be unable to confirm it from another (e.g. opening the email on a
/// phone).
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct ConfirmEmailChangePayload {
    pub token: Uuid,
}

/// Response for `GET /users/me/email/status`. A boolean only: the SMTP
/// settings themselves (host, account, sender) never leave the server.
///
/// Local rather than in `api-types` for the same reason as
/// `RequestEmailChangePayload`.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct EmailStatusResponse {
    /// Whether this server can send outgoing email at all. When false, every
    /// email channel (receipts, address verification) is inert.
    pub configured: bool,
}

fn email_status(sender: &dyn crate::services::EmailSender) -> EmailStatusResponse {
    EmailStatusResponse {
        configured: sender.is_configured(),
    }
}

/// Report whether this server can send email, so the client does not offer
/// email channels that would never deliver.
#[utoipa::path(
    get,
    path = "/users/me/email/status",
    tag = "users",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Whether outgoing email is configured", body = EmailStatusResponse),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn get_email_status<A>(
    AuthenticatedUser(_user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Json<EmailStatusResponse>
where
    A: SessionService + 'static,
{
    Json(email_status(&*state.email_sender))
}

/// Very small email-shape check.
///
/// `auth::service::validation::validate_email` does the same job but is
/// private to that crate, so this mirrors its rules rather than pulling in
/// the whole auth crate's validation surface for one check. Not exhaustive -
/// just enough to reject an obviously wrong address before minting a token
/// and an email for it.
pub(crate) fn looks_like_an_email(email: &str) -> bool {
    let email = email.trim();
    let parts: Vec<&str> = email.split('@').collect();
    let [local, domain] = parts.as_slice() else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// Start changing the authenticated account's email.
///
/// Requires a fresh passkey or wallet login (see `FreshlyAuthenticatedUser`),
/// not merely a valid session. Fails loudly rather than queuing a change
/// nobody can confirm: if SMTP is not configured on this server, the address
/// would sit pending forever with no error anywhere, which is exactly the
/// no-op-by-design behaviour that is right for a payment receipt and wrong
/// here.
#[utoipa::path(
    post,
    path = "/users/me/email",
    tag = "users",
    security(("bearer_auth" = [])),
    request_body = RequestEmailChangePayload,
    responses(
        (status = 202, description = "Verification email sent"),
        (status = 400, description = "Invalid email address"),
        (status = 401, description = "Unauthorized, or session is not fresh enough"),
        (status = 409, description = "Email already in use"),
        (status = 503, description = "Email is not configured on this server"),
    )
)]
pub async fn request_email_change<A>(
    FreshlyAuthenticatedUser(user): FreshlyAuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(payload): Json<RequestEmailChangePayload>,
) -> Result<StatusCode, (StatusCode, String)>
where
    A: SessionService + 'static,
{
    let new_email = payload.new_email.trim().to_string();

    if !looks_like_an_email(&new_email) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Not a valid email address.".to_string(),
        ));
    }

    if user
        .email
        .as_deref()
        .is_some_and(|current| current.eq_ignore_ascii_case(&new_email))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "That is already this account's email.".to_string(),
        ));
    }

    // Fail before creating any pending state: a no-op sender would otherwise
    // report success for a change nobody can ever confirm.
    if !state.email_sender.is_configured() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Email is not configured on this server, so an address change cannot be verified."
                .to_string(),
        ));
    }

    if let Some(existing) =
        auth::UserRepository::get_user_by_email(&*state.data_service, &new_email)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not check that email.".to_string(),
                )
            })?
        && existing.id != user.id
    {
        return Err((
            StatusCode::CONFLICT,
            "That email is already in use.".to_string(),
        ));
    }

    let expires_at = Utc::now() + EMAIL_CHANGE_TOKEN_TTL;
    let request = data_service::EmailChangeWriter::create_email_change_request(
        &*state.data_service,
        user.id,
        &new_email,
        expires_at,
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not start the email change.".to_string(),
        )
    })?;

    state
        .email_sender
        .send_email_change_verification(
            &new_email,
            &EmailChangeVerificationData {
                token: request.token.to_string(),
                expires_in_minutes: EMAIL_CHANGE_TOKEN_TTL.num_minutes(),
            },
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, user_id = %user.id.0, "failed to send email-change verification");
            (
                StatusCode::BAD_GATEWAY,
                "Could not send the verification email. Try again.".to_string(),
            )
        })?;

    Ok(StatusCode::ACCEPTED)
}

/// Confirm a pending email change with the code sent to the new address.
#[utoipa::path(
    post,
    path = "/users/me/email/confirm",
    tag = "users",
    request_body = ConfirmEmailChangePayload,
    responses(
        (status = 204, description = "Email changed"),
        (status = 400, description = "Invalid or expired verification code"),
        (status = 409, description = "Email already in use"),
    )
)]
pub async fn confirm_email_change<A>(
    State(state): State<PgAppState<A>>,
    Json(payload): Json<ConfirmEmailChangePayload>,
) -> Result<StatusCode, (StatusCode, String)>
where
    A: SessionService + 'static,
{
    let pending = data_service::EmailChangeWriter::consume_email_change_request(
        &*state.data_service,
        payload.token,
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not verify that code.".to_string(),
        )
    })?
    .ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "That verification code is invalid or has expired.".to_string(),
        )
    })?;

    // The full `User`, not `UserInfo`: `update_user` writes the whole record,
    // including the kdf/recovery fields `UserInfo` never carries, and it must
    // see them unchanged to accept the write at all.
    let mut current = auth::UserRepository::get_user(&*state.data_service, pending.user_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the account.".to_string(),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            "That account no longer exists.".to_string(),
        ))?;

    current.email = Some(pending.new_email);

    auth::UserRepository::update_user(&*state.data_service, &current)
        .await
        .map_err(|e| match e {
            auth::AuthError::UserExists(_) => (
                StatusCode::CONFLICT,
                "That email is already in use.".to_string(),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not update the account.".to_string(),
            ),
        })?;

    tracing::info!(user_id = %pending.user_id.0, "account email changed after verification");
    Ok(StatusCode::NO_CONTENT)
}

/// Whether removing the email is safe, and the refusal to send back if not.
///
/// Pulled out of `remove_email` as a pure function: the handler is generic
/// over `PgAppState<A>` and has no data-free way to exercise this guard, so
/// the rule that actually matters here needs a form a test can call without a
/// database.
///
/// Refuses when the account would be left with no wallet: with neither an
/// email nor a wallet, recovery falls back to the account id, which a
/// merchant may never have saved - so removal here would strand recovery
/// rather than merely narrow it. Mirrors `WalletAuthService::revoke_wallet`'s
/// symmetric guard against removing the last wallet from an account with no
/// email (`CannotRemovePrimaryWallet` in the auth crate).
fn email_removal_blocker(has_email: bool, has_wallet: bool) -> Option<(StatusCode, String)> {
    if !has_email {
        return Some((StatusCode::BAD_REQUEST, "No email is set.".to_string()));
    }

    if !has_wallet {
        return Some((
            StatusCode::CONFLICT,
            "Removing your email would leave this account with no way to recover it. \
             Add a wallet first, or keep the email."
                .to_string(),
        ));
    }

    None
}

/// Remove the authenticated account's email.
///
/// Refused when the account has no wallet: with neither an email nor a
/// wallet, recovery falls back to the account id, which a merchant may never
/// have saved - so removal here would strand recovery rather than merely
/// narrow it. Mirrors `WalletAuthService::revoke_wallet`'s symmetric guard
/// against removing the last wallet from an account with no email
/// (`CannotRemovePrimaryWallet` in the auth crate).
#[utoipa::path(
    delete,
    path = "/users/me/email",
    tag = "users",
    security(("bearer_auth" = [])),
    responses(
        (status = 204, description = "Email removed"),
        (status = 400, description = "No email set"),
        (status = 401, description = "Unauthorized, or session is not fresh enough"),
        (status = 409, description = "Removing the email would strand recovery"),
    )
)]
pub async fn remove_email<A>(
    FreshlyAuthenticatedUser(user): FreshlyAuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Result<StatusCode, (StatusCode, String)>
where
    A: SessionService + 'static,
{
    if let Some(err) =
        email_removal_blocker(user.email.is_some(), user.primary_wallet_address.is_some())
    {
        return Err(err);
    }

    let mut current = auth::UserRepository::get_user(&*state.data_service, user.id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the account.".to_string(),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            "That account no longer exists.".to_string(),
        ))?;

    current.email = None;

    auth::UserRepository::update_user(&*state.data_service, &current)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not update the account.".to_string(),
            )
        })?;

    tracing::info!(user_id = %user.id.0, "account email removed at its owner's request");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[path = "email_tests.rs"]
mod tests;
