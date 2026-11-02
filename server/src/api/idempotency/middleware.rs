//! The middleware entry point.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::IdempotencyState;
use super::cache::{CacheLookup, acquire_lock, cache_lookup, handle_and_cache};
use super::config::{CACHE_PREFIX, LOCK_PREFIX, MAX_REQUEST_BODY};
use super::key::{compute_hash, extract_scope, validate_key};
use super::response::{conflict_response, in_flight_response, invalid_key_response};

/// Idempotency middleware.
///
/// Only activates when the `Idempotency-Key` header is present on a POST request.
/// Passes through all other requests unchanged.
pub async fn middleware(
    State(state): State<Arc<IdempotencyState>>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() != Method::POST {
        return next.run(req).await;
    }
    let Some(key_header) = req.headers().get("idempotency-key").cloned() else {
        return next.run(req).await;
    };
    let Some(key_str) = validate_key(&key_header) else {
        return invalid_key_response();
    };
    // Missing auth — let the handler's auth extractor reject the request.
    let Some(scope) = extract_scope(&req) else {
        return next.run(req).await;
    };
    // Redis isn't connected yet (boot retry still in progress, see
    // `IdempotencyState::from_env`) or never has been - same fallback as a
    // live connection whose GET/SET fails: pass through unchanged, without
    // paying to buffer a body we won't use.
    let Some(mut conn) = state.connection() else {
        tracing::warn!("idempotency Redis not yet connected, falling through");
        super::record_health(false);
        return next.run(req).await;
    };

    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
        }
    };
    let request_hash = compute_hash(parts.method.as_str(), parts.uri.path(), &body_bytes);

    let cache_key = format!("{CACHE_PREFIX}:{scope}:{key_str}");
    let lock_key = format!("{LOCK_PREFIX}:{scope}:{key_str}");

    match cache_lookup(&mut conn, &cache_key, &request_hash).await {
        CacheLookup::Replay(resp) => return resp,
        CacheLookup::Conflict => return conflict_response(),
        CacheLookup::Miss => {}
        CacheLookup::Error(e) => {
            tracing::warn!(error = %e, "Redis GET failed for idempotency cache, falling through");
            let req = Request::from_parts(parts, Body::from(body_bytes));
            return next.run(req).await;
        }
    }

    if !acquire_lock(&mut conn, &lock_key).await {
        return in_flight_response();
    }

    let req = Request::from_parts(parts, Body::from(body_bytes));
    handle_and_cache(
        conn,
        cache_key,
        lock_key,
        request_hash,
        state.ttl_secs,
        req,
        next,
    )
    .await
}
