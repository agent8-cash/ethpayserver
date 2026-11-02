//! Redis-backed response cache and the in-flight lock.

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use super::config::{LOCK_TTL_SECS, MAX_RESPONSE_BODY};
use super::headers::collect_cacheable_headers;
use super::response::build_replay_response;

/// Cached response stored in Redis.
#[derive(Serialize, Deserialize)]
pub(super) struct CachedResponse {
    /// SHA-256 hash of the original request (method + path + body).
    pub(super) request_hash: String,
    /// HTTP status code.
    pub(super) status: u16,
    /// Response body bytes.
    pub(super) body: Vec<u8>,
    /// Response headers as (name, value) pairs. Non-UTF8 values are dropped on cache.
    #[serde(default)]
    pub(super) headers: Vec<(String, String)>,
}
/// Outcome of a cache GET.
pub(super) enum CacheLookup {
    /// Same key + same body → replay the cached response.
    Replay(Response),
    /// Same key + different body → 409.
    Conflict,
    /// No cached entry (or entry was malformed and treated as miss).
    Miss,
    /// Redis unreachable — caller should fall through to the live handler.
    Error(redis::RedisError),
}

/// Look up a cached response and classify it against the current request hash.
pub(super) async fn cache_lookup(
    conn: &mut redis::aio::MultiplexedConnection,
    cache_key: &str,
    request_hash: &str,
) -> CacheLookup {
    let cached: Option<Vec<u8>> = match redis::cmd("GET").arg(cache_key).query_async(conn).await {
        Ok(v) => v,
        Err(e) => {
            super::record_health(false);
            return CacheLookup::Error(e);
        }
    };
    super::record_health(true);
    let Some(bytes) = cached else {
        return CacheLookup::Miss;
    };
    let Ok(entry) = serde_json::from_slice::<CachedResponse>(&bytes) else {
        // Malformed cache entry — treat as miss so the handler runs fresh.
        return CacheLookup::Miss;
    };
    if entry.request_hash == request_hash {
        CacheLookup::Replay(build_replay_response(&entry))
    } else {
        CacheLookup::Conflict
    }
}

/// Try to acquire the in-flight lock via `SET NX EX`. Redis errors do not
/// block the request — idempotency is best-effort when Redis is unavailable.
pub(super) async fn acquire_lock(
    conn: &mut redis::aio::MultiplexedConnection,
    lock_key: &str,
) -> bool {
    match redis::cmd("SET")
        .arg(lock_key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(LOCK_TTL_SECS)
        .query_async::<Option<String>>(conn)
        .await
    {
        Ok(Some(_)) => {
            super::record_health(true);
            true
        }
        Ok(None) => {
            super::record_health(true);
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "Redis SET NX failed for idempotency lock, falling through");
            super::record_health(false);
            true
        }
    }
}

/// Release the in-flight lock. Best-effort — errors are ignored because the
/// lock will self-expire via TTL.
pub(super) async fn release_lock(conn: &mut redis::aio::MultiplexedConnection, lock_key: &str) {
    let _: Result<(), _> = redis::cmd("DEL").arg(lock_key).query_async(conn).await;
}

/// Run the downstream handler, then cache the response if it's a 2xx.
/// In all exit paths the in-flight lock is released.
pub(super) async fn handle_and_cache(
    mut conn: redis::aio::MultiplexedConnection,
    cache_key: String,
    lock_key: String,
    request_hash: String,
    ttl_secs: u64,
    req: Request,
    next: Next,
) -> Response {
    let response = next.run(req).await;

    if !response.status().is_success() {
        release_lock(&mut conn, &lock_key).await;
        return response;
    }

    let (resp_parts, resp_body) = response.into_parts();
    let resp_bytes = match axum::body::to_bytes(resp_body, MAX_RESPONSE_BODY).await {
        Ok(b) => b,
        Err(_) => {
            release_lock(&mut conn, &lock_key).await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let entry = CachedResponse {
        request_hash,
        status: resp_parts.status.as_u16(),
        body: resp_bytes.to_vec(),
        headers: collect_cacheable_headers(&resp_parts.headers),
    };

    if let Ok(json) = serde_json::to_vec(&entry) {
        let _: Result<(), _> = redis::cmd("SETEX")
            .arg(&cache_key)
            .arg(ttl_secs)
            .arg(&json)
            .query_async(&mut conn)
            .await;
    }
    release_lock(&mut conn, &lock_key).await;

    Response::from_parts(resp_parts, Body::from(resp_bytes))
}
