//! Lets a scoped service push a notice to an account holder, without ever
//! handing that service the address it lands on.
//!
//! Core, not a plugin: capability 7 (`crate::services::plugins::account_notice`)
//! is a *plugin* import, and a wasm module has no clock to drive anything on
//! a schedule from. This route gives the same capability to a service that
//! does have one, over plain HTTP, authenticated the same way
//! [`super::entitlements::push_entitlement`] is. Generic on purpose - this
//! host has no notion of what the caller is notifying about, the same way
//! `entitlements::push_entitlement` stores a standing without knowing which
//! plugin decided it.
//!
//! "No contact on file" is not a failure: it is the ordinary shape of a
//! wallet-only account, and the caller should neither retry it nor alert on
//! it. Delivery failure is - the caller is expected to retry on anything
//! other than 200.

use auth::SessionService;
use axum::{
    Json,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::MethodRouter,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::extractors::AccountNoticeSender;
use crate::services::email::AccountNotice;
use crate::services::plugins::{AccountNotifier, PluginAccountNotifier};
use crate::state::PgAppState;

/// Largest body accepted. The real payload is a short notice, not an essay.
pub const MAX_BODY_BYTES: usize = 8 * 1024;
const MAX_SUBJECT_BYTES: usize = 200;
const MAX_BODY_TEXT_BYTES: usize = 4096;

/// The exact refusal `notice_address` returns for an account with no usable
/// channel - pinned by a test there. Matched on here, rather than widening
/// `AccountNotifier`'s error type, to tell "nowhere to deliver to" apart from
/// a real failure without growing the capability's surface for one caller.
const NO_CHANNEL_MESSAGE: &str = "there is no channel this host can reach it through";

#[derive(Debug, Deserialize, ToSchema)]
pub struct PushAccountNoticeRequest {
    pub account_id: Uuid,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PushAccountNoticeResponse {
    /// False means the account has no channel this host can reach it
    /// through - not an error, and not something the caller should retry.
    pub delivered: bool,
}

fn validate(req: &PushAccountNoticeRequest) -> Result<(), &'static str> {
    if req.subject.trim().is_empty() || req.subject.len() > MAX_SUBJECT_BYTES {
        return Err("subject must be non-empty and at most 200 bytes");
    }
    if req.body.trim().is_empty() || req.body.len() > MAX_BODY_TEXT_BYTES {
        return Err("body must be non-empty and at most 4096 bytes");
    }
    Ok(())
}

/// Ask the host to notify an account holder.
///
/// Answers 200 whether or not the account had a channel to deliver through -
/// see [`PushAccountNoticeResponse::delivered`]. Only a lookup or transport
/// failure answers non-2xx, so the caller's retry-on-error policy never
/// retries the one outcome that is supposed to be silent.
#[utoipa::path(
    post,
    path = "/plugins/account-notices",
    tag = "plugins",
    security(("bearer_auth" = [])),
    request_body = PushAccountNoticeRequest,
    responses(
        (status = 200, description = "Delivered, or no channel to deliver through", body = PushAccountNoticeResponse),
        (status = 400, description = "Malformed body or field"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "The credential is not a key scoped to send account notices"),
        (status = 413, description = "Body too large"),
        (status = 502, description = "Could not read the account or deliver the notice; retry"),
    )
)]
pub async fn push_account_notice<A>(
    AccountNoticeSender(_sender): AccountNoticeSender,
    State(state): State<PgAppState<A>>,
    body: Bytes,
) -> Result<Json<PushAccountNoticeResponse>, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let req: PushAccountNoticeRequest = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed account notice body"))?;
    validate(&req).map_err(|m| (StatusCode::BAD_REQUEST, m))?;

    let notice = AccountNotice {
        subject: req.subject,
        body: req.body,
    };
    let notifier = PluginAccountNotifier::new(state);
    match notifier
        .notify_account(auth::UserId(req.account_id), &notice)
        .await
    {
        Ok(()) => Ok(Json(PushAccountNoticeResponse { delivered: true })),
        Err(e) if e.contains(NO_CHANNEL_MESSAGE) => {
            Ok(Json(PushAccountNoticeResponse { delivered: false }))
        }
        Err(e) => {
            tracing::error!(
                account_id = %req.account_id,
                error = %e,
                "could not deliver an account notice"
            );
            Err((StatusCode::BAD_GATEWAY, "could not deliver the notice"))
        }
    }
}

/// The route's method router, with the body bound the design requires.
pub fn route<A>() -> MethodRouter<PgAppState<A>>
where
    A: SessionService + 'static,
{
    axum::routing::post(push_account_notice::<A>).layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(subject: &str, body: &str) -> PushAccountNoticeRequest {
        PushAccountNoticeRequest {
            account_id: Uuid::nil(),
            subject: subject.into(),
            body: body.into(),
        }
    }

    #[test]
    fn a_well_formed_notice_validates() {
        assert!(validate(&req("subject", "body")).is_ok());
    }

    #[test]
    fn each_bad_field_is_refused() {
        assert!(validate(&req("", "body")).is_err());
        assert!(validate(&req("   ", "body")).is_err());
        assert!(validate(&req(&"x".repeat(201), "body")).is_err());
        assert!(validate(&req("subject", "")).is_err());
        assert!(validate(&req("subject", "   ")).is_err());
        assert!(validate(&req("subject", &"x".repeat(4097))).is_err());
    }
}
