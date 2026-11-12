//! Canned responses for the idempotency failure modes.

use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::cache::CachedResponse;

/// 409 response for an idempotency key reused with a different body.
pub(super) fn conflict_response() -> Response {
    (
        StatusCode::CONFLICT,
        axum::Json(serde_json::json!({"error": "idempotency_key_reuse"})),
    )
        .into_response()
}

/// 425 Too Early response when a concurrent request with the same key is
/// still in-flight.
pub(super) fn in_flight_response() -> Response {
    (
        #[allow(clippy::expect_used)] // 425 is always a valid status code
        StatusCode::from_u16(425).expect("425 is a valid status code"),
        axum::Json(serde_json::json!({"error": "idempotency_in_progress"})),
    )
        .into_response()
}
/// 400 response for invalid idempotency keys.
pub(super) fn invalid_key_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error": "idempotency_key_invalid"})),
    )
        .into_response()
}

/// 503 response when Redis is unavailable and idempotency cannot be guaranteed.
pub(super) fn redis_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "service_unavailable"})),
    )
        .into_response()
}
/// Build a response from cached data with `Idempotency-Replayed: true` header.
///
/// Restores every cached header (content-type, location, pagination, custom
/// x-\* headers). Hop-by-hop and server-generated headers are excluded at
/// cache time (see `NON_REPLAYABLE_HEADERS`), so the stored set is safe to
/// replay verbatim.
pub(super) fn build_replay_response(cached: &CachedResponse) -> Response {
    let status = StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder().status(status);

    for (name, value) in &cached.headers {
        builder = builder.header(name, value);
    }
    builder = builder.header("idempotency-replayed", "true");

    builder
        .body(Body::from(cached.body.clone()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
