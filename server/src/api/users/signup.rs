//! Choosing a plan: the merchant asks, this server asks the signup service.
//!
//! A POST, deliberately: it enrols the caller and creates an invoice, so it
//! must never be reachable by a link a browser might prefetch. The merchant
//! holds only their session here; the credential the signup service accepts is
//! minted per request by this server and never leaves it. See
//! `services::signup_service`.

use auth::SessionService;
use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::extractors::AuthenticatedUser;
use crate::state::PgAppState;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SignupRequest {
    pub plan_id: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SignupResponse {
    /// Where to send the merchant to pay. Absent when there is nothing to pay.
    pub checkout_url: Option<String>,
}

/// The 201 case. `None` means the answer cannot be trusted and is logged.
///
/// The service always sends the `checkout_url` key, `null` when nothing is
/// owed; an absent or non-string value is drift, not a free signup. A link we
/// refuse to pass on must likewise not look like "nothing to pay", since the
/// account is already enrolled and an invoice exists.
fn relay_created(body: &serde_json::Value) -> Option<SignupResponse> {
    let checkout_url = match body.get("checkout_url") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(u)) if u.starts_with("https://") => Some(u.clone()),
        _ => {
            tracing::error!("the signup service sent no usable checkout_url; withheld");
            return None;
        }
    };
    Some(SignupResponse { checkout_url })
}

/// What the merchant is told for each answer of the signup service. Only the
/// checkout link is passed through; nothing else the service says is relayed.
///
/// The merchant gets a deliberately coarse answer, so every status that is not
/// an ordinary refusal is logged here (status only, never the body): a 401 or
/// 403 means the service rejected our assertion, which is a wrong secret or
/// audience, and a 404 may be an unknown plan or a missing route - the service
/// gives no body to tell them apart.
fn relay(
    plan_id: i64,
    status: u16,
    body: &serde_json::Value,
) -> Result<SignupResponse, (StatusCode, &'static str)> {
    let unavailable = (
        StatusCode::BAD_GATEWAY,
        "signup is unavailable; try again later",
    );
    match status {
        201 => relay_created(body).ok_or(unavailable),
        404 => {
            tracing::warn!(
                status,
                "the signup service answered 404: an unknown plan, or a route it does not serve"
            );
            Err((StatusCode::NOT_FOUND, "no such plan"))
        }
        // The service refuses a second create. Its refusal carries the link
        // of the account's own unpaid first invoice when the reply to the
        // original signup was lost, so the merchant's retry can still pay. Only
        // for the plan that was asked for: another plan's link is never
        // presented as this one.
        409 => match relay_created(body) {
            Some(SignupResponse {
                checkout_url: Some(url),
            }) if body["plan_id"].as_i64() == Some(plan_id) => Ok(SignupResponse {
                checkout_url: Some(url),
            }),
            _ => Err((
                StatusCode::CONFLICT,
                "this account already has a subscription",
            )),
        },
        _ => {
            tracing::error!(
                status,
                "the signup service answered with an unexpected status"
            );
            Err(unavailable)
        }
    }
}

/// Subscribe the caller to a plan and return where to pay for it.
#[utoipa::path(
    post,
    path = "/users/me/signup",
    tag = "users",
    security(("bearer_auth" = [])),
    request_body = SignupRequest,
    responses(
        (status = 201, description = "Subscribed; pay at the link", body = SignupResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such plan, or signup is not configured"),
        (status = 409, description = "The account already has a subscription"),
        (status = 502, description = "The signup service is unavailable"),
    )
)]
pub async fn signup<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(request): Json<SignupRequest>,
) -> Result<(StatusCode, Json<SignupResponse>), (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let Some(service) = &state.signup_service else {
        return Err((StatusCode::NOT_FOUND, "signup is not available"));
    };
    // The account is the session's, never the request's: a merchant cannot
    // name another account because there is no field to name it in.
    match service.subscribe(user.id.0, request.plan_id).await {
        Ok((status, body)) => {
            relay(request.plan_id, status, &body).map(|r| (StatusCode::CREATED, Json(r)))
        }
        Err(error) => {
            tracing::error!(%error, "the signup service could not be reached");
            Err((
                StatusCode::BAD_GATEWAY,
                "signup is unavailable; try again later",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_https_checkout_link_is_passed_through() {
        let ok = relay(
            1,
            201,
            &json!({"checkout_url": "https://pay.example/checkout/1", "x": 1}),
        )
        .unwrap();
        assert_eq!(
            ok.checkout_url.as_deref(),
            Some("https://pay.example/checkout/1")
        );
        let none = relay(1, 201, &json!({"checkout_url": null})).unwrap();
        assert_eq!(none.checkout_url, None);
    }

    #[test]
    fn a_link_that_is_refused_or_a_201_without_a_body_is_a_bad_gateway() {
        for body in [
            json!({"checkout_url": "javascript:alert(1)"}),
            json!({"checkout_url": "http://pay.example/1"}),
            json!(null),
            json!({}),
            json!({"checkout_url": 7}),
        ] {
            assert_eq!(relay(1, 201, &body).unwrap_err().0, StatusCode::BAD_GATEWAY);
        }
    }

    #[test]
    fn a_refusal_that_carries_the_pending_link_hands_it_over() {
        let ok = relay(
            1,
            409,
            &json!({"checkout_url": "https://pay.example/checkout/1", "plan_id": 1}),
        )
        .unwrap();
        assert_eq!(
            ok.checkout_url.as_deref(),
            Some("https://pay.example/checkout/1")
        );
        // A pending link for a different plan is a refusal, not a success.
        let other = relay(
            1,
            409,
            &json!({"checkout_url": "https://pay.example/checkout/1", "plan_id": 2}),
        );
        assert_eq!(other.unwrap_err().0, StatusCode::CONFLICT);
        let bad = relay(1, 409, &json!({"checkout_url": "http://pay.example/1"}));
        assert_eq!(bad.unwrap_err().0, StatusCode::CONFLICT);
    }

    #[test]
    fn refusals_map_and_anything_else_is_a_bad_gateway() {
        for body in [json!(null), json!({"checkout_url": null})] {
            assert_eq!(relay(1, 409, &body).unwrap_err().0, StatusCode::CONFLICT);
        }
        assert_eq!(
            relay(1, 404, &json!(null)).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        for status in [200, 401, 403, 500, 503] {
            assert_eq!(
                relay(1, status, &json!(null)).unwrap_err().0,
                StatusCode::BAD_GATEWAY
            );
        }
    }
}
