//! Choosing a plan: the merchant asks, this server asks the signup service.
//!
//! A POST, deliberately: it enrols the caller and creates an invoice, so it
//! must never be reachable by a link a browser might prefetch. The merchant
//! holds only their session here; the credential the signup service accepts is
//! minted per request by this server and never leaves it. See
//! `services::signup_service`.

use auth::SessionService;
use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use utoipa::ToSchema;

use crate::api::extractors::AuthenticatedUser;
use crate::api::rate_limit;
use crate::services::signup_service::SignupPlan;
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

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanListResponse {
    pub plans: Vec<SignupPlan>,
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
        // The service refuses a second create. Its refusal carries the link of
        // the account's own unpaid first invoice when the reply to the original
        // signup was lost, so the merchant's retry can still pay.
        //
        // THIS DOES NOT CHECK THAT THE LINK IS FOR THE PLAN THAT WAS ASKED FOR,
        // and it cannot. A comparison against the body's `plan_id` stood here and
        // was deleted: the service builds that field from the plan the caller
        // REQUESTED, not the one the subscription is actually on, so the check
        // compared the requested plan against itself and was true for every
        // input. It read as protection while providing none, which is worse than
        // its absence - a later reader would have trusted it.
        //
        // The residual hazard is real and is not ours to close here: a merchant
        // already on one plan who retries naming another receives that other
        // plan's id beside the first plan's payment link. The wrong field is the
        // service's, and the fix belongs with it.
        409 => match relay_created(body) {
            Some(SignupResponse {
                checkout_url: Some(url),
            }) => Ok(SignupResponse {
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
    headers: axum::http::HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
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
    // The merchant's address, read the way the rate limiter reads it, so the
    // signup service can attribute the signup to the merchant and not to this
    // server's peer.
    let client_ip = rate_limit::client_ip(&headers, peer.ip());
    match service
        .subscribe(user.id.0, request.plan_id, client_ip)
        .await
    {
        Ok((status, body)) => relay(status, &body).map(|r| (StatusCode::CREATED, Json(r))),
        Err(error) => {
            tracing::error!(%error, "the signup service could not be reached");
            Err((
                StatusCode::BAD_GATEWAY,
                "signup is unavailable; try again later",
            ))
        }
    }
}

/// List the plans the caller may choose from.
#[utoipa::path(
    get,
    path = "/users/me/plans",
    tag = "users",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "The plans on offer", body = PlanListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Signup is not available"),
    )
)]
pub async fn list_plans<A>(
    AuthenticatedUser(_user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Result<Json<PlanListResponse>, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let Some(service) = &state.signup_service else {
        return Err((StatusCode::NOT_FOUND, "signup is not available"));
    };
    Ok(Json(PlanListResponse {
        plans: service.plans.clone(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_https_checkout_link_is_passed_through() {
        let ok = relay(
            201,
            &json!({"checkout_url": "https://pay.example/checkout/1", "x": 1}),
        )
        .unwrap();
        assert_eq!(
            ok.checkout_url.as_deref(),
            Some("https://pay.example/checkout/1")
        );
        let none = relay(201, &json!({"checkout_url": null})).unwrap();
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
            assert_eq!(relay(201, &body).unwrap_err().0, StatusCode::BAD_GATEWAY);
        }
    }

    #[test]
    fn a_refusal_that_carries_the_pending_link_hands_it_over() {
        let ok = relay(
            409,
            &json!({"checkout_url": "https://pay.example/checkout/1", "plan_id": 1}),
        )
        .unwrap();
        assert_eq!(
            ok.checkout_url.as_deref(),
            Some("https://pay.example/checkout/1")
        );
        // THERE IS NO "DIFFERENT PLAN" CASE TO ASSERT. A case here fed
        // `plan_id: 2` against a request for plan 1 and asserted a refusal,
        // which passed - but the service builds that field from the plan the
        // caller REQUESTED, so it never emits a body where the two differ. The
        // case tested an input the service cannot produce, and the comparison it
        // exercised has been deleted with it.
        //
        // What a merchant on another plan actually receives is the first plan's
        // payment link beside the requested plan's id. That is the service's
        // field to fix and is tracked there; nothing here can tell the two
        // plans apart.
        let bad = relay(409, &json!({"checkout_url": "http://pay.example/1"}));
        assert_eq!(bad.unwrap_err().0, StatusCode::CONFLICT);
    }

    #[test]
    fn refusals_map_and_anything_else_is_a_bad_gateway() {
        for body in [json!(null), json!({"checkout_url": null})] {
            assert_eq!(relay(409, &body).unwrap_err().0, StatusCode::CONFLICT);
        }
        assert_eq!(
            relay(404, &json!(null)).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        for status in [200, 401, 403, 500, 503] {
            assert_eq!(
                relay(status, &json!(null)).unwrap_err().0,
                StatusCode::BAD_GATEWAY
            );
        }
    }
}
