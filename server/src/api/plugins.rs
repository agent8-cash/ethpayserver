//! Mounts installed plugins' declared routes under the reserved
//! `/plugins/{id}` prefix, with the host's own authentication
//! applied before any plugin code runs.
//!
//! [`PluginRegistry`] is the load-time gate for which manifests
//! this host accepts; it knows nothing about HTTP. This module is what turns
//! "registered" into "reachable": each plugin's own router is nested under
//! its [`PluginId`], which is validated (no path separators, no empty
//! labels) as exactly what makes that prefix safe to reserve — see
//! `payserver_plugin_api::PluginId`. The `/plugins` prefix itself is applied
//! by [`router()`], not left to whoever calls it, so the reservation is
//! structural rather than a convention a future call site could forget.
//!
//! Two segments are reserved under a plugin's prefix, and the split is not
//! cosmetic:
//!
//! - `/plugins/{id}/pages/{path}` serves the [`PageElement`] tree a plugin
//!   returns from its `render_page` export. It is mounted by
//!   [`super::router`], not here, and [`get_page`] is its handler.
//! - `/plugins/{id}/routes/...` is the plugin's own declared router, nested
//!   by [`router()`] below.
//!
//! Without that second segment they would overlap, and overlap here fails
//! silently in the worse direction: axum prefers a static nest to a dynamic
//! route, so a plugin that declared any router at all would shadow the
//! host's page endpoint for its own id, and a merchant would get that
//! plugin's 404 where their billing page should be. Giving each a segment of
//! its own means neither can reach the other, whichever gets mounted first.
//!
//! Nothing in this build can ask a loaded plugin for its own router: the
//! wasmtime runtime instantiates and calls plugins, but no entry point
//! produces a router. `declared_routes` is therefore supplied by the caller
//! — today always empty in the live server — as the seam a future slice
//! fills in once a plugin can actually produce one. Pages do not go through
//! it and never did.
//!
//! Auth is a [`middleware::from_fn_with_state`] layer wrapped around each
//! plugin's *entire* nest, not a per-handler extractor a plugin's own code
//! could omit: a plugin must not get a say in who is authenticated, and a
//! layer runs before the plugin's router at all, including for a sub-path
//! the plugin itself does not recognize. It reuses [`AuthenticatedUser`],
//! the exact extractor every core handler already goes through, so a plugin
//! is held to the same authentication as the rest of the host. The
//! authenticated `UserInfo` — identity and role — is then inserted into
//! the request's extensions before the plugin's router runs, so a future
//! plugin handler can read who is calling via `Extension<UserInfo>` instead
//! of asserting it itself.
//!
//! A plugin id absent from [`PluginRegistry`] has nothing mounted for its
//! prefix: a request under it falls through to the app's ordinary 404,
//! never to a core route and never a 500.

use std::collections::HashMap;

use auth::{Role, SessionService};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
};
use payserver_plugin_api::{Manifest, PluginId};

use super::ApiErr;
use super::extractors::AuthenticatedUser;
use crate::services::plugins::{PageElement, PageError, PageRequest, PluginRegistry, Viewer};
use crate::state::PgAppState;

/// Builds the `/plugins` mount.
///
/// `declared_routes` pairs a plugin id with the router it wants exposed.
/// Only ids also present in `registry` are mounted — an entry here for an
/// id the registry never accepted is dropped, not trusted. The `/plugins`
/// prefix is applied here, not by the caller, so the reservation holds no
/// matter how this router gets merged into the app.
pub fn router<A>(
    state: PgAppState<A>,
    registry: &PluginRegistry,
    declared_routes: HashMap<PluginId, Router>,
) -> Router
where
    A: SessionService + 'static,
{
    let mut mounted = Router::new();
    for (id, plugin_routes) in declared_routes {
        if registry.get(&id).is_none() {
            continue;
        }
        let gated = plugin_routes.layer(middleware::from_fn_with_state(
            state.clone(),
            require_host_auth,
        ));
        mounted = mounted.nest(&format!("/{id}/routes"), gated);
    }

    Router::new().nest("/plugins", mounted)
}

/// The host's own authentication, run before a plugin's router ever sees the
/// request. Threads the authenticated identity and role through as a request
/// extension so a plugin handler can read who is calling instead of deciding
/// it itself.
async fn require_host_auth(
    AuthenticatedUser(user): AuthenticatedUser,
    mut request: Request,
    next: Next,
) -> Response {
    request.extensions_mut().insert(user);
    next.run(request).await
}

/// The host resolves this from the authenticated identity; the plugin never
/// chooses it. `Role` only distinguishes admin from everyone else today, so
/// every non-admin session is a merchant view.
fn viewer_for(role: Role) -> Viewer {
    match role {
        Role::ServerAdmin => Viewer::Admin,
        Role::User => Viewer::Merchant,
    }
}

impl From<PageError> for ApiErr {
    fn from(err: PageError) -> Self {
        match err {
            // A plugin that trapped, timed out or is disabled is not a page
            // that does not exist. 404 for a billing page that is merely
            // broken is a far more convincing lie than 502, and it sends
            // whoever is debugging it looking for a routing mistake.
            PageError::Unavailable(_) => (
                StatusCode::BAD_GATEWAY,
                "the plugin could not render this page".to_string(),
            )
                .into(),
            PageError::PluginNotFound | PageError::PageNotFound => {
                (StatusCode::NOT_FOUND, err.to_string()).into()
            }
        }
    }
}

/// The plugin whose manifest claims `slug`, if any.
///
/// Reads the installed list rather than a cached map: installs are rare, page
/// requests are not hot enough for it to matter, and a cache would need
/// invalidating on install, uninstall and enable - three chances to serve a
/// page from a plugin that is no longer the one behind that URL.
async fn resolve_plugin<A>(state: &PgAppState<A>, slug: &str) -> Option<PluginId>
where
    A: SessionService + 'static,
{
    // Refuse the lookup outright if it is not a well-formed slug, so a
    // request for `../..` never reaches a manifest comparison.
    let slug = payserver_plugin_api::PluginSlug::new(slug).ok()?;

    let installed =
        data_service::InstalledPluginReader::list_installed_plugins(&*state.data_service)
            .await
            .ok()?;

    installed.into_iter().find_map(|row| {
        let manifest: Manifest = row.manifest_toml.parse().ok()?;
        (manifest.slug.as_ref() == Some(&slug)).then_some(manifest.id)
    })
}

/// Which declared pages a caller is offered.
///
/// Its own function so the rule can be tested without a database behind it.
/// Navigation only: see [`list_plugin_pages`] for why this is not what stops
/// a merchant reading an operator's page.
fn visible_pages(
    declared: Vec<payserver_plugin_api::PageDeclaration>,
    is_admin: bool,
) -> Vec<PluginPageInfo> {
    declared
        .into_iter()
        // `admin_only` or an admin-settings placement - either is enough.
        // A page placed in admin settings is admin-only by definition (there
        // is nowhere else it is reachable from), so a plugin should not have
        // to say it twice, and a merchant must not be offered it because the
        // author said it only once.
        .filter(|page| is_admin || !(page.admin_only || page.placement.is_admin_only()))
        .map(|page| PluginPageInfo {
            path: page.path,
            label: page.label,
            icon: page.icon.into(),
        })
        .collect()
}

/// One plugin's pages, as the client should list them.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PluginPagesInfo {
    /// The plugin's identity. Stable, and what admin surfaces name.
    pub id: String,
    /// What its pages live under in a URL. Short, and what a client builds
    /// links from - `/billing/subscriptions`, never
    /// `/cash.random.billing/subscriptions`.
    pub slug: String,
    pub pages: Vec<PluginPageInfo>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PluginPageInfo {
    /// Append to `/plugins/{id}/pages/` to fetch it.
    pub path: String,
    pub label: String,
    pub icon: PluginPageIcon,
}

/// The icon a plugin's page shows in navigation, mirrored from
/// [`payserver_plugin_api::PageIcon`] for the sake of an API schema:
/// `payserver-plugin-api` does not depend on `utoipa`, and a fixed,
/// API-facing vocabulary is a better reason to add that dependency there
/// than this one field.
///
/// The consumer is `payserver-client`, a separate repository: its
/// `PluginLinks` (`src/app/layout.rs`) maps each variant here to a component
/// in `src/app/icons.rs`. That mapping cannot live in this diff - it is a
/// different crate in a different repo - so it ships as its own commit and
/// PR there, opened alongside this one.
///
/// `payserver-billing`'s own manifest still needs `icon = "card"` added now
/// that this vocabulary exists, so its sidebar entry stops using the
/// fallback too. That repo is private and isn't checked out anywhere this
/// change can reach it from, so it is not done here - tracked as a follow-up
/// against that repo instead of silently left undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginPageIcon {
    Card,
    Coins,
    Chart,
    Users,
    Shield,
    Bell,
    Key,
    Tag,
    Plug,
}

impl From<payserver_plugin_api::PageIcon> for PluginPageIcon {
    fn from(icon: payserver_plugin_api::PageIcon) -> Self {
        use payserver_plugin_api::PageIcon;
        match icon {
            PageIcon::Card => Self::Card,
            PageIcon::Coins => Self::Coins,
            PageIcon::Chart => Self::Chart,
            PageIcon::Users => Self::Users,
            PageIcon::Shield => Self::Shield,
            PageIcon::Bell => Self::Bell,
            PageIcon::Key => Self::Key,
            PageIcon::Tag => Self::Tag,
            PageIcon::Plug => Self::Plug,
        }
    }
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PluginPagesResponse {
    pub plugins: Vec<PluginPagesInfo>,
}

/// What a client should put in its navigation.
///
/// Declared in each plugin's manifest, not discovered by calling it: a menu
/// must be built before any page is asked for, and a host that ran wasm to
/// find out what to draw a sidebar from would be running plugin code on
/// every page load of the app.
///
/// Only plugins this process actually has loaded are listed. An installed
/// but unloaded plugin's page would 404 on arrival, and offering a merchant
/// a menu entry that cannot open is worse than not offering it.
///
/// `admin_only` entries are filtered out here for a merchant. That is
/// navigation, not access control - the plugin still decides what to return
/// for the viewer it is handed, and the viewer still comes from the session.
/// Filtering here only avoids showing someone a door they would be handed
/// their own page through anyway.
#[utoipa::path(
    get,
    path = "/plugins",
    responses((status = 200, body = PluginPagesResponse)),
    tag = "plugins"
)]
pub async fn list_plugin_pages<A>(
    State(state): State<PgAppState<A>>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> Result<Json<PluginPagesResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    use data_service::InstalledPluginReader;

    let installed = InstalledPluginReader::list_installed_plugins(&*state.data_service)
        .await
        .map_err(|e| {
            ApiErr::from((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read installed plugins: {e}"),
            ))
        })?;

    let is_admin = matches!(user.role, Role::ServerAdmin);

    let plugins = installed
        .into_iter()
        .filter(|row| row.enabled)
        .filter_map(|row| {
            let id = PluginId::new(row.id.clone()).ok()?;
            // Loaded, not merely installed: a page from a plugin this
            // process never instantiated has nothing to render it.
            let loaded = state
                .plugin_host
                .as_ref()
                .and_then(|h| h.status(&id))
                .is_some_and(|s| s.enabled);
            if !loaded {
                return None;
            }

            let manifest: Manifest = row.manifest_toml.parse().ok()?;
            // No slug, no URL to offer. A manifest with pages and no slug
            // cannot parse, so this is a plugin that declared neither.
            let slug = manifest.slug?.to_string();
            let pages = visible_pages(manifest.pages, is_admin);

            // A plugin with no pages a caller may see is not listed at all,
            // rather than listed empty - a client should not have to know
            // that an empty list means "draw nothing".
            if pages.is_empty() {
                return None;
            }
            Some(PluginPagesInfo {
                id: row.id,
                slug,
                pages,
            })
        })
        .collect();

    Ok(Json(PluginPagesResponse { plugins }))
}

pub async fn get_page<A>(
    State(state): State<PgAppState<A>>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path((plugin_ref, path)): Path<(String, String)>,
) -> Result<Json<PageElement>, ApiErr>
where
    A: SessionService + 'static,
{
    // Addressed by slug, with the id still accepted. A URL a person reads
    // uses the slug; an admin tool, a log line and an older client all have
    // the id, and breaking those to make the URL pretty would be a poor
    // trade. Slug first, because that is the one a plugin chose and the one
    // that must win if a plugin ever picks a slug that looks like an id.
    let plugin_id = match resolve_plugin(&state, &plugin_ref).await {
        Some(id) => id,
        None => PluginId::new(plugin_ref)
            .map_err(|e| ApiErr::from((StatusCode::NOT_FOUND, e.to_string())))?,
    };

    // Both halves of "who is asking" are resolved here, from the session the
    // host authenticated, and neither is anything the request claimed. A
    // plugin that could name its own viewer could ask to be treated as an
    // admin, and one that could name an account could read another
    // merchant's page.
    let request = PageRequest {
        path,
        viewer: viewer_for(user.role),
        account_id: Some(user.id.0.to_string()),
    };

    let page = state.plugin_pages.render(&plugin_id, &request).await?;
    Ok(Json(page))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-only assertions")]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use auth::{
        AuthError, DeviceId, Result as AuthResult, Role, Session, SessionId, UserId, UserInfo,
    };
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

        let plugin_routes =
            Router::new().route("/api/invoices", get(|| async { "plugin invoices" }));
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
}
