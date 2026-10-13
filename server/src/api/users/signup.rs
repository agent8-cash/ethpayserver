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

/// What the merchant is told for each answer of the signup service. Only the
/// checkout link is passed through; nothing else the service says is relayed.
fn relay(
    status: u16,
    body: &serde_json::Value,
) -> Result<SignupResponse, (StatusCode, &'static str)> {
    match status {
        201 => Ok(SignupResponse {
            checkout_url: body["checkout_url"]
                .as_str()
                .filter(|u| u.starts_with("https://"))
                .map(str::to_string),
        }),
        404 => Err((StatusCode::NOT_FOUND, "no such plan")),
        409 => Err((
            StatusCode::CONFLICT,
            "this account already has a subscription",
        )),
        _ => Err((
            StatusCode::BAD_GATEWAY,
            "signup is unavailable; try again later",
        )),
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
        let odd = relay(201, &json!({"checkout_url": "javascript:alert(1)"})).unwrap();
        assert_eq!(odd.checkout_url, None);
    }

    #[test]
    fn refusals_map_and_anything_else_is_a_bad_gateway() {
        assert_eq!(
            relay(409, &json!(null)).unwrap_err().0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            relay(404, &json!(null)).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        for status in [401, 403, 500, 503] {
            assert_eq!(
                relay(status, &json!(null)).unwrap_err().0,
                StatusCode::BAD_GATEWAY
            );
        }
    }
}
