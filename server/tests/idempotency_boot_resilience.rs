#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Proves the two properties `IdempotencyState`'s background Redis retry
//! and health signal exist for, both through the real entry point - a POST
//! sent through the actual `idempotency::middleware` layer - rather than by
//! inspecting internal state:
//!
//! 1. A Redis outage at the moment the control is built does not disable it
//!    for the rest of the process's life: once Redis answers, it mounts
//!    itself and starts replaying cached responses, with no restart.
//! 2. `idempotency::is_healthy()` (and the `ethpayserver_idempotency_healthy`
//!    gauge it mirrors) tracks that in both directions - unhealthy while
//!    unmounted, healthy once a GET/SET NX actually succeeds.
//!
//! Needs a real Redis. An unset `TEST_REDIS_URL` returns early without
//! running anything, same convention as this crate's other Redis-gated
//! tests; CI's `test` job sets it and runs ignored tests.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use server::api::idempotency::{IdempotencyState, is_healthy, middleware};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tower::util::ServiceExt;

/// A token long enough to pass `extract_scope`'s 32-char minimum - the
/// request's tenant scope, not a credential anything here checks.
const SCOPE_TOKEN: &str = "00000000-0000-0000-0000-000000000000";

/// Binds an ephemeral port, reads back its address, then immediately drops
/// the listener - freeing the port but leaving the address with nothing
/// listening on it. Connecting there fails fast with "connection refused"
/// (no RESP handshake to hang on), the same observable shape as Redis being
/// down.
async fn unbound_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// A running TCP proxy. Dropping this leaves it running (`JoinHandle`'s
/// default); call [`Self::stop`] to actually sever every connection it is
/// currently forwarding, the only way to make an already-established
/// `MultiplexedConnection` fail without a cooperating fake Redis.
struct Proxy {
    accept_task: JoinHandle<()>,
    forwarding_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Proxy {
    /// Stop accepting new connections and abort every connection currently
    /// being forwarded. Aborting drops the task's `TcpStream`s, closing
    /// both sockets - the client side (whatever `IdempotencyState` is
    /// holding) sees its connection die the next time it tries to use it.
    fn stop(self) {
        self.accept_task.abort();
        for task in self.forwarding_tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

/// Forwards every connection accepted on `listener` to `upstream`
/// byte-for-byte: stands in for Redis "coming back up" at an address
/// `IdempotencyState` is already holding open, without this test needing to
/// speak the Redis protocol itself.
fn spawn_proxy(listener: TcpListener, upstream: SocketAddr) -> Proxy {
    let forwarding_tasks = Arc::new(Mutex::new(Vec::new()));
    let tasks_for_accept_loop = Arc::clone(&forwarding_tasks);
    let accept_task = tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                return;
            };
            let task = tokio::spawn(async move {
                if let Ok(mut outbound) = TcpStream::connect(upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
            tasks_for_accept_loop.lock().unwrap().push(task);
        }
    });
    Proxy {
        accept_task,
        forwarding_tasks,
    }
}

/// `TEST_REDIS_URL` as the plain `host:port` a raw TCP proxy connects to.
fn upstream_addr(redis_url: &str) -> SocketAddr {
    let without_scheme = redis_url.split("://").nth(1).unwrap_or(redis_url);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    host_port.parse().unwrap_or_else(|e| {
        panic!("TEST_REDIS_URL must be host:port, e.g. redis://127.0.0.1:6379: {e}")
    })
}

/// A minimal app: one POST route behind the real idempotency middleware,
/// counting how many times the route itself actually ran.
fn test_app(state: Arc<IdempotencyState>, calls: Arc<AtomicUsize>) -> Router {
    Router::new()
        .route(
            "/invoices",
            post(move || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::CREATED, "{\"id\":\"inv_1\"}")
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(state, middleware))
}

fn idempotent_request(key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/invoices")
        .header("idempotency-key", key)
        .header("authorization", format!("Bearer {SCOPE_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a local Redis instance; set TEST_REDIS_URL, e.g. redis://127.0.0.1:6379"]
async fn boot_outage_then_recovery_mounts_without_a_restart() {
    let Some(real_redis_url) = std::env::var("TEST_REDIS_URL").ok() else {
        return;
    };
    let upstream = upstream_addr(&real_redis_url);

    // Redis is "down": the state is pointed at a port nothing is listening
    // on, same as a boot-time outage.
    let proxy_addr = unbound_addr().await;
    let state = Arc::new(
        IdempotencyState::from_env(&format!("redis://{proxy_addr}"))
            .expect("from_env should not fail just because Redis is unreachable"),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let app = test_app(Arc::clone(&state), Arc::clone(&calls));

    let key = format!("boot-outage-{}", uuid::Uuid::new_v4());
    let resp = app.clone().oneshot(idempotent_request(&key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        resp.headers().get("idempotency-replayed").is_none(),
        "control isn't mounted yet - must pass straight through, not replay"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        !is_healthy(),
        "control must report unhealthy while it has never connected"
    );

    // Redis "comes back": a proxy starts listening on the exact address the
    // state has been holding onto, forwarding to the real TEST_REDIS_URL -
    // no restart, no new state, the same `app` as above.
    let listener = TcpListener::bind(proxy_addr).await.unwrap();
    let _proxy = spawn_proxy(listener, upstream);

    // `IdempotencyState`'s background retry polls on a fixed interval;
    // drive real requests through the real entry point until one comes
    // back replayed, bounded well past that interval.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut mounted = false;
    while Instant::now() < deadline {
        let resp = app.clone().oneshot(idempotent_request(&key)).await.unwrap();
        if resp.headers().get("idempotency-replayed").is_some() {
            mounted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        mounted,
        "idempotency control never mounted after Redis recovered - a boot-time \
         outage would disable it for the rest of the process's life"
    );
    assert!(
        is_healthy(),
        "control mounted but did not report healthy once Redis actually worked"
    );

    // The replay just observed proves the cache round-trip, not just the
    // connection: send the same key once more and confirm the route itself
    // does not run again.
    let calls_before = calls.load(Ordering::SeqCst);
    let resp = app.clone().oneshot(idempotent_request(&key)).await.unwrap();
    assert_eq!(resp.headers().get("idempotency-replayed").unwrap(), "true");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before,
        "handler ran again instead of being replayed from cache"
    );
}

#[tokio::test]
#[ignore = "requires a local Redis instance; set TEST_REDIS_URL, e.g. redis://127.0.0.1:6379"]
async fn a_live_connections_get_failure_falls_through_and_reports_unhealthy() {
    let Some(real_redis_url) = std::env::var("TEST_REDIS_URL").ok() else {
        return;
    };
    let upstream = upstream_addr(&real_redis_url);

    // Mount against a real, working proxy first.
    let proxy_addr = unbound_addr().await;
    let listener = TcpListener::bind(proxy_addr).await.unwrap();
    let proxy = spawn_proxy(listener, upstream);

    let state = Arc::new(
        IdempotencyState::from_env(&format!("redis://{proxy_addr}"))
            .expect("from_env should not fail against a reachable address"),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let app = test_app(Arc::clone(&state), Arc::clone(&calls));

    let key = format!("live-failure-{}", uuid::Uuid::new_v4());
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !is_healthy() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(is_healthy(), "never mounted against a working proxy");

    // One real round trip to confirm the connection actually works before
    // breaking it on purpose.
    let resp = app.clone().oneshot(idempotent_request(&key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Break the guarantee on purpose: sever the proxy connection, so the
    // connection `IdempotencyState` is already holding can no longer reach
    // Redis. No new connect attempt follows - this is a live connection
    // failing, the second of the ticket's three granularities (the first
    // was boot-time unmounted, above; the third, a lock SET NX failure,
    // shares this same "connection no longer works" cause and so isn't
    // exercised separately).
    proxy.stop();
    // Give the severed TCP connection a moment to actually surface as a
    // read/write error rather than racing the next request against it.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Same key, same body - a cache GET over the now-dead connection, which
    // must fall through rather than error the request, and must flip the
    // health signal back down.
    let resp = app.clone().oneshot(idempotent_request(&key)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "a Redis GET failure must fall through to the real handler, not fail the request"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "falling through must still run the real handler"
    );
    assert!(
        !is_healthy(),
        "a live connection's GET failure must flip the signal back to unhealthy"
    );
}
