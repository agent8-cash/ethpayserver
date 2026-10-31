use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use auth::{AuthError, DeviceId, Result as AuthResult, Role, Session, SessionId, UserId, UserInfo};
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::routing::get;
use data_service::PgDataService;
use payserver_plugin_api::{Manifest, Version};
use rates::NoOpRateProvider;
use tower::ServiceExt;

use super::*;

/// The only session id `FakeSessions` accepts. Anything else fails the
/// same way an unknown or expired session does in production.
fn valid_session_id() -> SessionId {
    SessionId(uuid::Uuid::from_u128(42))
}

struct FakeSessions;

#[async_trait]
impl SessionService for FakeSessions {
    async fn validate_session(&self, session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        if session_id != valid_session_id() {
            return Err(AuthError::InvalidCredentials);
        }
        let user_id = UserId(uuid::Uuid::from_u128(1));
        let device_id = DeviceId(uuid::Uuid::from_u128(2));
        let user = UserInfo {
            id: user_id,
            email: Some("merchant@example.com".to_string()),
            primary_wallet_address: None,
            created_at: chrono::Utc::now(),
            last_login_at: None,
            role: Role::User,
        };
        Ok((user, Session::new(user_id, device_id)))
    }

    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        Ok(())
    }

    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        Ok(())
    }

    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        Ok(0)
    }
}

/// A `PgAppState` whose auth is `FakeSessions` and whose data service
/// wraps a lazily-connecting pool — no query ever runs against it on the
/// paths these tests exercise, so no real Postgres is needed.
fn test_state() -> PgAppState<FakeSessions> {
    let pool = sqlx::PgPool::connect_lazy("postgres://localhost/test").unwrap();
    PgAppState::new(
        Arc::new(PgDataService::new(pool)),
        Arc::new(FakeSessions),
        None,
        Arc::new(NoOpRateProvider),
        Arc::new(crate::services::email::NoopEmailSender),
    )
}

fn bearer_for_valid_session() -> String {
    format!("Bearer {}", valid_session_id().0)
}

fn registry_with(id: &str) -> PluginRegistry {
    let mut registry = PluginRegistry::new(Version::parse("1.2.5").unwrap());
    let manifest: Manifest = format!(
        r#"
            id = "{id}"
            version = "0.1.0"
            dependencies = ["ethpayserver:^1.2.0"]
            kind = "action"
        "#
    )
    .parse()
    .unwrap();
    registry.register(manifest).unwrap();
    registry
}

/// Ticket test 1: an unauthenticated request to a plugin route is
/// refused by the host before any plugin code runs.
#[tokio::test]
async fn unauthenticated_request_is_refused_before_plugin_runs() {
    let registry = registry_with("cash.random.billing");

    let invoked = Arc::new(AtomicBool::new(false));
    let invoked_in_handler = Arc::clone(&invoked);
    let plugin_routes = Router::new().route(
        "/{*rest}",
        get(move || {
            let invoked = Arc::clone(&invoked_in_handler);
            async move {
                invoked.store(true, Ordering::SeqCst);
                "plugin ran"
            }
        }),
    );

    let mut declared_routes = HashMap::new();
    declared_routes.insert(PluginId::new("cash.random.billing").unwrap(), plugin_routes);

    let app = router(test_state(), &registry, declared_routes);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/routes/anything")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !invoked.load(Ordering::SeqCst),
        "plugin handler must not run before the host authenticates the request"
    );
}

/// Ticket test 2: a plugin id that is not installed returns 404, not a
/// 500 and not a core handler.
#[tokio::test]
async fn plugin_id_not_installed_404s() {
    let registry = PluginRegistry::new(Version::parse("1.2.5").unwrap());
    let declared_routes = HashMap::new();

    let app = router(test_state(), &registry, declared_routes);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/routes/anything")
        .header("authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A plugin id addressed without the reserved `/plugins` prefix must not
/// resolve to anything — the prefix is applied by `router()` itself, not
/// left as a convention for whoever merges this router into the app.
#[tokio::test]
async fn plugin_id_without_the_plugins_prefix_404s() {
    let registry = registry_with("cash.random.billing");
    let plugin_routes = Router::new().route("/", get(|| async { "should not be reachable" }));
    let mut declared_routes = HashMap::new();
    declared_routes.insert(PluginId::new("cash.random.billing").unwrap(), plugin_routes);

    let app = router(test_state(), &registry, declared_routes);

    let request = HttpRequest::builder()
        .uri("/cash.random.billing/")
        .header("authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// An id the caller declared routes for, but the registry never
/// accepted, is treated the same as "not installed" — the registry is
/// the source of truth, not the presence of a router.
#[tokio::test]
async fn declared_routes_for_an_unregistered_id_are_not_mounted() {
    let registry = PluginRegistry::new(Version::parse("1.2.5").unwrap());
    let plugin_routes = Router::new().route("/", get(|| async { "should not be reachable" }));
    let mut declared_routes = HashMap::new();
    declared_routes.insert(PluginId::new("cash.random.billing").unwrap(), plugin_routes);

    let app = router(test_state(), &registry, declared_routes);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/routes/")
        .header("authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Ticket test 3: a plugin declaring a route that would collide with a
/// core path lands under its own prefix and does not intercept the core
/// route.
#[tokio::test]
async fn plugin_route_does_not_shadow_a_colliding_core_path() {
    let registry = registry_with("cash.random.billing");

    let plugin_routes = Router::new().route("/api/invoices", get(|| async { "plugin invoices" }));
    let mut declared_routes = HashMap::new();
    declared_routes.insert(PluginId::new("cash.random.billing").unwrap(), plugin_routes);

    let core = Router::new().route("/api/invoices", get(|| async { "core invoices" }));
    let app = core.merge(router(test_state(), &registry, declared_routes));

    let core_response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/invoices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(core_response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(core_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body, "core invoices");

    let plugin_response = app
        .oneshot(
            HttpRequest::builder()
                .uri("/plugins/cash.random.billing/routes/api/invoices")
                .header("authorization", bearer_for_valid_session())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(plugin_response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(plugin_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body, "plugin invoices");
}

/// The host passes the authenticated identity and role to the plugin
/// rather than letting it assert who is calling: `require_host_auth`
/// inserts the `UserInfo` it extracted into the request's extensions, so
/// a plugin handler downstream can read it with `Extension<UserInfo>`.
#[tokio::test]
async fn authenticated_identity_and_role_reach_the_plugin() {
    let registry = registry_with("cash.random.billing");

    let plugin_routes = Router::new().route(
        "/whoami",
        get(
            |axum::extract::Extension(user): axum::extract::Extension<UserInfo>| async move {
                format!("{}:{:?}", user.id.0, user.role)
            },
        ),
    );
    let mut declared_routes = HashMap::new();
    declared_routes.insert(PluginId::new("cash.random.billing").unwrap(), plugin_routes);

    let app = router(test_state(), &registry, declared_routes);

    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri("/plugins/cash.random.billing/routes/whoami")
                .header("authorization", bearer_for_valid_session())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let expected = format!("{}:{:?}", uuid::Uuid::from_u128(1), Role::User);
    assert_eq!(body, expected.as_str());
}

/// Ablation for ticket test 3: remove the prefixing and the collision is
/// no longer avoidable. Axum refuses to build a router with two GET
/// handlers for the same path — this is the concrete failure the
/// `/plugins/{id}` prefix in `router()` exists to prevent.
#[test]
fn without_the_reserved_prefix_the_core_route_does_not_survive() {
    let plugin_routes: Router =
        Router::new().route("/api/invoices", get(|| async { "plugin invoices" }));
    let core: Router = Router::new().route("/api/invoices", get(|| async { "core invoices" }));

    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| core.merge(plugin_routes)));

    assert!(
        result.is_err(),
        "merging an unprefixed plugin router onto the core router should panic on the exact \
         path collision that the /plugins/{{id}} prefix exists to prevent"
    );
}

mod pages;
mod render;
