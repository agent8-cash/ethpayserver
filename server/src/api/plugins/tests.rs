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

/// The page endpoint, through the router a request actually reaches.
///
/// The route has been mounted for a while and always answered 404,
/// because `PageHost` was built empty and nothing ever registered a
/// renderer in it. That is indistinguishable from a feature that was
/// never wired up, and it was one: a unit test of `PageHost` passes
/// either way. This asks over HTTP, with a renderer registered, so it
/// fails if either half goes missing.
#[tokio::test]
async fn a_registered_renderer_is_reachable_over_http() {
    use payserver_plugin_api::page::{Badge, Tone};
    use payserver_plugin_host::{PageHost, PageRenderError, PageRenderer};

    struct Billing;

    #[async_trait]
    impl PageRenderer for Billing {
        async fn render_page(
            &self,
            request: &PageRequest,
        ) -> Result<Option<PageElement>, PageRenderError> {
            if request.path != "subscriptions" {
                return Ok(None);
            }
            // Echoes both halves of the identity back, so the test can
            // assert the host resolved them rather than the plugin.
            Ok(Some(PageElement::Badge(Badge {
                text: format!(
                    "{:?}/{}",
                    request.viewer,
                    request.account_id.as_deref().unwrap_or("none")
                ),
                tone: Tone::Info,
            })))
        }
    }

    let mut pages = PageHost::new();
    pages.register(
        PluginId::new("cash.random.billing").unwrap(),
        Arc::new(Billing),
    );

    let mut state = test_state();
    state.plugin_pages = Arc::new(pages);

    let app = Router::new()
        .route(
            "/plugins/{id}/pages/{*path}",
            axum::routing::get(get_page::<FakeSessions>),
        )
        .with_state(state);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/pages/subscriptions")
        .header("Authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a registered renderer must be reachable through the mounted route"
    );

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let element: PageElement = serde_json::from_slice(&body).unwrap();
    let PageElement::Badge(badge) = element else {
        panic!("expected the badge the renderer returned");
    };
    assert_eq!(
        badge.text,
        format!("Merchant/{}", uuid::Uuid::from_u128(1)),
        "both the viewer and the account must come from the authenticated \
         session, not from anything the request claimed"
    );

    // A path the plugin does not serve is still a 404, so the test above
    // is not passing because everything answers 200.
    let missing = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/pages/not-a-page")
        .header("Authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(missing).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

/// `Figure` survives the round trip through the page endpoint as
/// `type: figure`, not the `#[serde(other)] Unknown` fallback.
///
/// Same shape as `a_registered_renderer_is_reachable_over_http`, but for
/// the element this commons pin adds: on an older pin a plugin page
/// containing a `figure` deserialized into `Unknown` server-side and
/// re-serialized as `{"type":"unknown"}`, silently dropping the label,
/// value and tone a dashboard tile needs. Ablation: pin
/// `payserver-commons` back to a revision before `PageElement::Figure`
/// existed and this test fails because the type does not compile, let
/// alone round-trip.
#[tokio::test]
async fn a_figure_round_trips_through_the_page_endpoint_as_figure_not_unknown() {
    use payserver_plugin_api::page::{Figure, Tone};
    use payserver_plugin_host::{PageHost, PageRenderError, PageRenderer};

    struct Billing;

    #[async_trait]
    impl PageRenderer for Billing {
        async fn render_page(
            &self,
            request: &PageRequest,
        ) -> Result<Option<PageElement>, PageRenderError> {
            if request.path != "summary" {
                return Ok(None);
            }
            Ok(Some(PageElement::Figure(Figure {
                label: "Lapsed".to_string(),
                value: "7".to_string(),
                tone: Tone::Danger,
            })))
        }
    }

    let mut pages = PageHost::new();
    pages.register(
        PluginId::new("cash.random.billing").unwrap(),
        Arc::new(Billing),
    );

    let mut state = test_state();
    state.plugin_pages = Arc::new(pages);

    let app = Router::new()
        .route(
            "/plugins/{id}/pages/{*path}",
            axum::routing::get(get_page::<FakeSessions>),
        )
        .with_state(state);

    let request = HttpRequest::builder()
        .uri("/plugins/cash.random.billing/pages/summary")
        .header("Authorization", bearer_for_valid_session())
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();

    // Asserted on the raw JSON first: this is the exact failure mode the
    // old pin produced — a 200 whose body is `{"type":"unknown"}`, which
    // would still deserialize into `PageElement::Unknown` below and could
    // be mistaken for a passing test if only the typed value were
    // checked.
    let raw: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        raw["type"], "figure",
        "a figure element must serialize as type: figure, not fall back to unknown; got {raw}"
    );

    let element: PageElement = serde_json::from_slice(&body).unwrap();
    let PageElement::Figure(figure) = element else {
        panic!("expected the figure the renderer returned, got {element:?}");
    };
    assert_eq!(figure.label, "Lapsed");
    assert_eq!(figure.value, "7");
    assert_eq!(figure.tone, Tone::Danger);
}

/// A plugin that could not answer is a 502, not a 404. Answering "no
/// such page" for a billing page that merely trapped sends whoever is
/// debugging it looking for a routing mistake that is not there.
#[test]
fn a_broken_plugin_is_a_bad_gateway_not_a_missing_page() {
    use axum::response::IntoResponse;
    use payserver_plugin_host::PageRenderError;

    let unavailable: ApiErr = PageError::Unavailable(PageRenderError::new("wasm trap")).into();
    let missing: ApiErr = PageError::PageNotFound.into();

    assert_eq!(
        unavailable.into_response().status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(missing.into_response().status(), StatusCode::NOT_FOUND);
}

#[test]
fn server_admin_is_the_admin_viewer() {
    assert_eq!(viewer_for(Role::ServerAdmin), Viewer::Admin);
}

/// Navigation is filtered by role, and the order a plugin declared is
/// the order a menu is built in.
#[test]
fn admin_only_pages_are_offered_only_to_an_admin() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![
        PageDeclaration {
            path: "subscription".to_string(),
            label: "Subscription".to_string(),
            placement: PagePlacement::Nav,
            icon: PageIcon::Plug,
            admin_only: false,
        },
        PageDeclaration {
            path: "subscriptions".to_string(),
            label: "Subscriptions".to_string(),
            placement: PagePlacement::Nav,
            icon: PageIcon::Plug,
            admin_only: true,
        },
    ];

    let merchant = visible_pages(declared.clone(), false);
    assert_eq!(merchant.len(), 1);
    assert_eq!(merchant[0].path, "subscription");

    let admin = visible_pages(declared, true);
    assert_eq!(admin.len(), 2);
    assert_eq!(
        admin.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        vec!["subscription", "subscriptions"],
        "the plugin's declared order is what a menu is built from"
    );
}

/// A page placed in admin settings is admin-only by definition - there is
/// nowhere else it is reachable from. A plugin should not have to say so
/// twice, and a merchant must not be offered it because the author said
/// it only once.
#[test]
fn an_admin_settings_page_is_admin_only_without_saying_so_twice() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![PageDeclaration {
        path: "subscriptions".to_string(),
        label: "All merchants".to_string(),
        placement: PagePlacement::AdminSettings,
        icon: PageIcon::Plug,
        // Deliberately NOT set: the placement alone must be enough.
        admin_only: false,
    }];

    assert!(
        visible_pages(declared.clone(), false).is_empty(),
        "a merchant must not be offered a page about other merchants"
    );
    assert_eq!(visible_pages(declared, true).len(), 1);
}

/// The icon a plugin declares is what a client is told to draw, not
/// whatever the generic default happens to be - otherwise the field
/// could parse correctly in the manifest and still never reach anyone
/// reading the response.
#[test]
fn a_declared_icon_reaches_the_response() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![PageDeclaration {
        path: "subscription".to_string(),
        label: "Subscription".to_string(),
        placement: PagePlacement::Nav,
        icon: PageIcon::Card,
        admin_only: false,
    }];

    let pages = visible_pages(declared, false);
    assert_eq!(pages[0].icon, PluginPageIcon::Card);
}

/// A manifest predating this field, or one written against a newer icon
/// vocabulary than this build knows, both land on [`PageIcon::Plug`]
/// before `visible_pages` ever sees them (that defaulting and fallback
/// is `payserver-plugin-api`'s own, and is tested there). What this repo
/// owns is the mapping onto [`PluginPageIcon`], so this only needs to
/// confirm the default variant survives that mapping unchanged.
#[test]
fn a_page_with_the_default_icon_maps_to_the_generic_one() {
    use payserver_plugin_api::{PageDeclaration, PageIcon, PagePlacement};

    let declared = vec![PageDeclaration {
        path: "subscription".to_string(),
        label: "Subscription".to_string(),
        placement: PagePlacement::Nav,
        icon: PageIcon::default(),
        admin_only: false,
    }];

    let pages = visible_pages(declared, false);
    assert_eq!(pages[0].icon, PluginPageIcon::Plug);
}

#[test]
fn everyone_else_is_the_merchant_viewer() {
    assert_eq!(viewer_for(Role::User), Viewer::Merchant);
}

/// A slug resolves to the plugin that declared it, and the id still
/// works - an admin tool, a log line and an older client all carry ids,
/// and breaking those to make a URL pretty would be a poor trade.
#[test]
fn a_manifest_declares_the_slug_a_url_is_built_from() {
    let manifest: Manifest = r#"
        id = "cash.random.billing"
        version = "0.1.0"
        dependencies = ["ethpayserver:^1.2.0"]
        kind = "filter"
        slug = "billing"

        [[pages]]
        path = "subscriptions"
        label = "Subscriptions"
    "#
    .parse()
    .unwrap();

    assert_eq!(manifest.slug.as_ref().unwrap().as_str(), "billing");
    assert_eq!(
        manifest.id.as_str(),
        "cash.random.billing",
        "the id is unchanged - the slug is an addition, not a rename"
    );
}

/// The lookup validates before it compares, so a traversal attempt never
/// reaches a manifest at all. Without this, `..` would be compared
/// against every installed plugin's slug - harmless today, and exactly
/// the sort of input that should be refused at the door rather than
/// relied on to match nothing.
#[test]
fn a_malformed_slug_is_refused_before_any_lookup() {
    use payserver_plugin_api::PluginSlug;

    for bad in ["..", "../admin", "bil/ling", "Billing", ""] {
        assert!(
            PluginSlug::new(bad).is_err(),
            "{bad:?} must not survive to be compared against a manifest"
        );
    }
    assert!(PluginSlug::new("billing").is_ok());
}
