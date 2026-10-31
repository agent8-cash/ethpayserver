//! Per-tier, IP-keyed rate limiting middleware.
//!
//! Classifies requests into tiers (auth, write, read, ws, health) and applies
//! separate governor-backed rate limiters per tier per client IP.
//!
//! # Environment Variables
//!
//! - `RATE_LIMIT_AUTH`  - Auth endpoint limit, req/min (default: 30)
//! - `RATE_LIMIT_WRITE` - Write endpoint limit, req/min (default: 120)
//! - `RATE_LIMIT_READ`  - Read endpoint limit, req/min (default: 300)
//! - `RATE_LIMIT_WS`    - WebSocket upgrade limit, req/min (default: 60)

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use governor::{
    Quota, RateLimiter,
    clock::{Clock, DefaultClock},
    state::keyed::DefaultKeyedStateStore,
};

use crate::metrics;

/// IP-keyed rate limiter.
type KeyedLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// Rate limit configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Max requests per minute for auth endpoints.
    pub auth_rpm: u32,
    /// Max requests per minute for write endpoints.
    pub write_rpm: u32,
    /// Max requests per minute for read endpoints.
    pub read_rpm: u32,
    /// Max WebSocket upgrades per minute.
    pub ws_rpm: u32,
}

impl Default for RateLimitConfig {
    /// Numbers a payment processor can actually run on.
    ///
    /// Every limiter here is keyed by **client IP**, which is what made the
    /// previous set wrong rather than merely strict. A carrier NAT, an
    /// office, a hotel - all of it arrives as one address, so a limit written
    /// as though it were per-user is spent by whoever else is behind the same
    /// router.
    ///
    /// These are still limits. They bound a flood; they no longer bound a
    /// business.
    fn default() -> Self {
        Self {
            // Deliberately the tightest, and still tripled. This is the
            // credential surface, so being generous has a real cost - but ten
            // a minute also locks out an office where three people sign in at
            // once, and the ceremony is WebAuthn rather than a password, so
            // there is no secret here to grind against.
            auth_rpm: 30,
            // A store creating an invoice per customer does a write per sale.
            // Twenty a minute caps how fast a merchant may trade, which is
            // not a security property.
            write_rpm: 120,
            // One dashboard load fans out across several endpoints, so sixty
            // was a handful of page loads a minute shared by everyone on that
            // address - and the checkout page now polls, deliberately, at six
            // a minute.
            read_rpm: 300,
            // One long-lived socket per checkout, plus reconnects. The number
            // has to cover a page that is *meant* to reconnect, not merely a
            // page that is opened once.
            ws_rpm: 60,
        }
    }
}

impl RateLimitConfig {
    /// Load from environment variables with defaults.
    pub fn from_env() -> Self {
        Self {
            auth_rpm: parse_env_u32("RATE_LIMIT_AUTH", 30),
            write_rpm: parse_env_u32("RATE_LIMIT_WRITE", 120),
            read_rpm: parse_env_u32("RATE_LIMIT_READ", 300),
            ws_rpm: parse_env_u32("RATE_LIMIT_WS", 60),
        }
    }
}

/// Per-tier rate limiters.
pub struct RateLimitState {
    auth: KeyedLimiter,
    auth_rpm: u32,
    write: KeyedLimiter,
    write_rpm: u32,
    read: KeyedLimiter,
    read_rpm: u32,
    ws: KeyedLimiter,
    ws_rpm: u32,
}

impl RateLimitState {
    /// Create limiters from configuration.
    pub fn from_config(config: &RateLimitConfig) -> Self {
        Self {
            auth: make_limiter(config.auth_rpm),
            auth_rpm: config.auth_rpm,
            write: make_limiter(config.write_rpm),
            write_rpm: config.write_rpm,
            read: make_limiter(config.read_rpm),
            read_rpm: config.read_rpm,
            ws: make_limiter(config.ws_rpm),
            ws_rpm: config.ws_rpm,
        }
    }
}

/// Request classification tier.
#[derive(Debug, Clone, Copy)]
enum Tier {
    Health,
    Auth,
    Write,
    Read,
    WebSocket,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Health => "health",
            Tier::Auth => "auth",
            Tier::Write => "write",
            Tier::Read => "read",
            Tier::WebSocket => "ws",
        }
    }
}

/// Classify a request into a rate limit tier.
fn classify(path: &str, method: &Method) -> Tier {
    if path.starts_with("/health")
        || path == "/metrics"
        || path.starts_with("/swagger")
        || path.starts_with("/api-docs")
    {
        Tier::Health
    } else if path.starts_with("/auth") {
        Tier::Auth
    } else if path == "/ws" || path.ends_with("/ws") {
        // `ends_with`, not equality. The socket tier covered exactly one
        // route - the authenticated dashboard's `/ws`. The public checkout
        // socket is mounted at `/api/checkout/ws` and was classified as an
        // ordinary read, so the one upgrade endpoint reachable without a
        // session, on the page where customers pay, was bounded by the read
        // budget rather than the socket one.
        //
        // That is backwards. A read is cheap and finishes; an upgrade holds a
        // connection open, and the unauthenticated one is the one worth
        // bounding.
        Tier::WebSocket
    } else if *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS {
        Tier::Read
    } else {
        Tier::Write
    }
}

/// Extract client IP from proxy headers, falling back to peer address.
pub(crate) fn client_ip(headers: &axum::http::HeaderMap, fallback: IpAddr) -> IpAddr {
    if let Some(xff) = headers.get("x-forwarded-for")
        && let Ok(s) = xff.to_str()
        && let Some(first) = s.split(',').next()
        && let Ok(ip) = first.trim().parse::<IpAddr>()
    {
        return ip;
    }
    if let Some(xri) = headers.get("x-real-ip")
        && let Ok(s) = xri.to_str()
        && let Ok(ip) = s.trim().parse::<IpAddr>()
    {
        return ip;
    }
    fallback
}

/// Rate limiting middleware.
///
/// Classifies each request by tier, applies the matching per-IP limiter,
/// and returns 429 with Retry-After header when the limit is exceeded.
pub async fn middleware(
    State(limiters): State<Arc<RateLimitState>>,
    req: Request,
    next: Next,
) -> Response {
    let tier = classify(req.uri().path(), req.method());

    let (limiter, rpm) = match tier {
        Tier::Health => return next.run(req).await,
        Tier::Auth => (&limiters.auth, limiters.auth_rpm),
        Tier::Write => (&limiters.write, limiters.write_rpm),
        Tier::Read => (&limiters.read, limiters.read_rpm),
        Tier::WebSocket => (&limiters.ws, limiters.ws_rpm),
    };

    let fallback_addr = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let ip = client_ip(req.headers(), fallback_addr);

    match limiter.check_key(&ip) {
        Ok(_) => next.run(req).await,
        Err(not_until) => {
            tracing::warn!(
                tier = tier.label(),
                ip = %ip,
                limit_rpm = rpm,
                "rate limit exceeded"
            );
            metrics::record_rate_limited(tier.label());
            let wait = not_until.wait_time_from(DefaultClock::default().now());
            let retry_after = (wait.as_secs() + 1).to_string();
            let mut response = (StatusCode::TOO_MANY_REQUESTS, "Too many requests").into_response();
            if let Ok(val) = retry_after.parse() {
                response.headers_mut().insert("retry-after", val);
            }
            response
        }
    }
}

fn make_limiter(rpm: u32) -> KeyedLimiter {
    let rpm = NonZeroU32::new(rpm).unwrap_or(NonZeroU32::MIN);
    RateLimiter::keyed(Quota::per_minute(rpm))
}

fn parse_env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn classify_health_endpoints() {
        assert!(matches!(classify("/health", &Method::GET), Tier::Health));
        assert!(matches!(
            classify("/health/live", &Method::GET),
            Tier::Health
        ));
        assert!(matches!(classify("/metrics", &Method::GET), Tier::Health));
        assert!(matches!(
            classify("/swagger-ui", &Method::GET),
            Tier::Health
        ));
        assert!(matches!(
            classify("/api-docs/openapi.json", &Method::GET),
            Tier::Health
        ));
    }

    #[test]
    fn classify_auth_endpoints() {
        assert!(matches!(
            classify("/auth/register", &Method::POST),
            Tier::Auth
        ));
        assert!(matches!(
            classify("/auth/login/complete", &Method::POST),
            Tier::Auth
        ));
    }

    #[test]
    fn classify_websocket() {
        assert!(matches!(classify("/ws", &Method::GET), Tier::WebSocket));
    }

    #[test]
    fn classify_read_vs_write() {
        assert!(matches!(classify("/invoices", &Method::GET), Tier::Read));
        assert!(matches!(classify("/stores", &Method::GET), Tier::Read));
        assert!(matches!(classify("/invoices", &Method::POST), Tier::Write));
        assert!(matches!(classify("/stores/abc", &Method::PUT), Tier::Write));
        assert!(matches!(
            classify("/stores/abc", &Method::DELETE),
            Tier::Write
        ));
    }

    #[test]
    fn config_defaults() {
        let config = RateLimitConfig::default();
        assert_eq!(config.auth_rpm, 30);
        assert_eq!(config.write_rpm, 120);
        assert_eq!(config.read_rpm, 300);
        assert_eq!(config.ws_rpm, 60);
    }

    /// The public checkout socket must be bounded as a socket.
    ///
    /// It is mounted at `/api/checkout/ws`, and the classifier matched `/ws`
    /// exactly - so the only upgrade endpoint reachable without a session was
    /// counted against the read budget. A read is cheap and finishes; an
    /// upgrade holds a connection open.
    #[test]
    fn every_socket_upgrade_is_classified_as_one() {
        for path in ["/ws", "/api/ws", "/checkout/ws", "/api/checkout/ws"] {
            assert!(
                matches!(classify(path, &Method::GET), Tier::WebSocket),
                "{path} is a socket upgrade and must be bounded as one"
            );
        }
        // An ordinary read is still a read, including one that merely
        // contains the letters.
        assert!(matches!(
            classify("/api/invoices", &Method::GET),
            Tier::Read
        ));
        assert!(matches!(classify("/api/wsx", &Method::GET), Tier::Read));
    }

    /// A socket budget has to cover a page that is meant to reconnect: five
    /// clients behind one address, each running a backoff sequence once.
    #[test]
    fn the_socket_budget_survives_reconnects_from_several_clients() {
        let ws = RateLimitConfig::default().ws_rpm;
        assert!(
            ws >= 30,
            "ws_rpm {ws} leaves no room for several clients behind one NAT to reconnect"
        );
    }

    /// A write limit below a sale a second is a cap on trading, not security.
    #[test]
    fn the_write_budget_is_not_a_cap_on_selling() {
        assert!(RateLimitConfig::default().write_rpm >= 60);
    }

    #[test]
    fn limiter_allows_within_quota() {
        let limiter = make_limiter(10);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(limiter.check_key(&ip).is_ok());
    }

    #[test]
    fn limiter_rejects_burst_over_quota() {
        let limiter = make_limiter(2);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(limiter.check_key(&ip).is_ok());
        assert!(limiter.check_key(&ip).is_ok());
        assert!(limiter.check_key(&ip).is_err());
    }

    #[test]
    fn different_ips_are_independent() {
        let limiter = make_limiter(1);
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(limiter.check_key(&ip1).is_ok());
        assert!(limiter.check_key(&ip2).is_ok());
        assert!(limiter.check_key(&ip1).is_err());
        assert!(limiter.check_key(&ip2).is_err());
    }

    #[test]
    fn health_not_rate_limited_label() {
        assert_eq!(Tier::Health.label(), "health");
        assert_eq!(Tier::Auth.label(), "auth");
        assert_eq!(Tier::Write.label(), "write");
        assert_eq!(Tier::Read.label(), "read");
        assert_eq!(Tier::WebSocket.label(), "ws");
    }

    /// Captures `tracing` output into a shared buffer so a test can assert on
    /// log lines without a full logging setup.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn as_string(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// A rate limit rejection is silent to anyone watching from outside the
    /// process. This asserts it is not silent to the logs.
    #[tokio::test]
    async fn rate_limit_exceeded_logs_a_warning() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let logs = CapturedLogs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let state = Arc::new(RateLimitState::from_config(&RateLimitConfig {
            auth_rpm: 1,
            write_rpm: 1,
            read_rpm: 1,
            ws_rpm: 1,
        }));
        let app: Router = Router::new()
            .route("/auth/login", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, middleware));

        let build_request = || {
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .body(Body::empty())
                .unwrap()
        };

        let first = app.clone().oneshot(build_request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(build_request()).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);

        let output = logs.as_string();
        assert!(
            output.contains("rate limit exceeded"),
            "expected a warning about the rejected request, got: {output}"
        );
        assert!(
            output.contains("auth"),
            "expected the tier that fired to be logged, got: {output}"
        );
    }
}
