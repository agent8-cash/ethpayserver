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
mod tests;
