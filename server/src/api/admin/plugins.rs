//! The plugin lifecycle over HTTP: install, upgrade, enable, disable,
//! uninstall, and the record of who did what.
//!
//! Until this existed a plugin could only be installed by writing an
//! `installed_plugins` row and placing an artifact on disk by hand - which is
//! precisely the "an admin should never need filesystem access" failure this
//! whole area exists to avoid.
//!
//! ## Installing does not load
//!
//! An install writes the artifact, the row and an audit event, and stops
//! there. The plugin loads on the next boot. This is a deliberate copy of
//! BTCPay's model: hot-reload is a large complication for something done
//! rarely, and swapping a wasmtime instance underneath a request in flight is
//! exactly the kind of machinery that fails at 3am. Every mutating response
//! therefore carries `restart_required`, and it is the honest answer rather
//! than a limitation to be papered over.
//!
//! Disabling is the exception, and the asymmetry is on purpose: an admin
//! switching off a misbehaving plugin means *stop running it now*. That one
//! takes effect in the live process as well as in the database.
//!
//! ## The upload is JSON, not multipart
//!
//! A wasm module is binary, and multipart is the conventional shape for it.
//! It would mean adding `multer` to the dependency tree for one endpoint,
//! where base64 in a JSON body is already expressible with a crate the tree
//! carries, is trivial to drive from a test or a script without an encoder,
//! and keeps the request a single atomic document - manifest and module
//! together, so there is no state where one arrived and the other did not.
//! The cost is a third more bytes on the wire for an operation performed
//! rarely.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use base64::Engine as _;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use auth::SessionService;
use data_service::{
    InstalledPluginReader, InstalledPluginWriter, NewInstalledPlugin, NewPluginEvent,
    PluginEventKind,
};
use payserver_plugin_api::{Manifest, PluginId};

use crate::api::ApiErr;
use crate::api::extractors::AdminAuth;
use crate::services::plugins::{
    CancelSubscriptionOutcome, PluginArtifacts, PluginRegistry, PluginStorage, cancel_subscription,
    generate_role_password, host_version,
};
use crate::state::PgAppState;

/// The largest wasm module this endpoint accepts, before base64.
///
/// Not a security boundary - the caller is already an authenticated server
/// admin - but an unbounded upload into memory is an unbounded upload into
/// memory. 32 MiB is far above any plausible plugin and far below anything
/// that would trouble the process.
const MAX_WASM_BYTES: usize = 32 * 1024 * 1024;

/// The newest page-descriptor schema the client this server ships can draw.
///
/// A manifest asking for a newer one is refused at install: the alternative
/// is a plugin that installs cleanly and then shows blank or placeholder
/// panels, which is the worst way to learn the client is behind. Raise this
/// together with the client pin, in the same change.
const SUPPORTED_UI_SCHEMA: u32 = 1;

/// Refuse a manifest whose page schema this server's client cannot draw.
fn check_ui_schema(requested: Option<u32>) -> Result<(), String> {
    match requested {
        Some(v) if v > SUPPORTED_UI_SCHEMA => Err(format!(
            "plugin needs page schema {v}, but this server's client draws up to \
             {SUPPORTED_UI_SCHEMA}; update the server's client before installing it"
        )),
        _ => Ok(()),
    }
}

// ============================================================================
// Types
// ============================================================================

/// One installed plugin, as an admin needs to see it.
///
/// Carries `enabled` and `loaded` separately because they answer different
/// questions and routinely disagree. `enabled` is what the database records
/// and what the next boot will honour; `loaded` is whether this process has
/// a live, non-disabled instance right now. A plugin that is enabled but not
/// loaded is either a safe-mode boot, a plugin installed since this process
/// started, or one that has failed since startup - and collapsing those into
/// one field is how an admin ends up restarting a server to fix something a
/// restart will not fix.
///
/// `loaded` deliberately does not say *running*. It means the host compiled
/// and instantiated the module and would dispatch to it - but nothing in
/// this build dispatches to a plugin at all: `run_action` and `run_filter`
/// have no callers outside the host's own tests, and the one wired call site
/// (`invoice_creation_filters`, consulted on invoice creation) is populated
/// in tests and never in the live server. Calling this field `running` would
/// tell an admin their plugin is doing something, when what is true is that
/// it loaded and is waiting for a dispatch path that does not exist yet.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminPluginInfo {
    pub id: String,
    pub version: String,
    /// What the install record says about the next boot.
    pub enabled: bool,
    /// Whether this process holds a live, enabled instance right now. Not a
    /// claim that any request path invokes it - see the type's docs.
    pub loaded: bool,
    /// Why it is off, when something turned it off.
    pub disabled_reason: Option<String>,
    /// Consecutive failed calls, from the host. `0` when it is not loaded.
    pub consecutive_failures: u32,
    pub installed_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

/// The installed-plugins list.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminPluginListResponse {
    pub plugins: Vec<AdminPluginInfo>,
    /// Repeated from `GET /admin/safe-mode` so the list is self-explaining:
    /// without it, every plugin reading `enabled: true, loaded: false` looks
    /// like a fleet of crashes rather than one flag.
    pub safe_mode: bool,
}

/// Refuse an install whose slug another plugin already owns.
///
/// Uniqueness cannot live on the slug type: that validates one slug and knows
/// nothing about any other. Only the host knows what else is installed, so
/// this is the one place it can be enforced - and it has to be, because two
/// plugins sharing a slug means one silently owns the URL and the other's
/// pages become unreachable.
async fn refuse_a_taken_slug<A>(
    state: &PgAppState<A>,
    manifest: &Manifest,
    id: &PluginId,
) -> Result<(), ApiErr>
where
    A: SessionService + 'static,
{
    let Some(slug) = &manifest.slug else {
        return Ok(());
    };

    let installed = InstalledPluginReader::list_installed_plugins(&*state.data_service)
        .await
        .map_err(|e| server_error(format!("could not read installed plugins: {e}")))?;

    for row in &installed {
        // An upgrade of this same plugin keeps its own slug.
        if row.id == id.as_str() {
            continue;
        }
        // A stored manifest that no longer parses cannot be compared, and is
        // not a reason to refuse an unrelated install: it is already broken
        // and reported as such at boot.
        let Ok(other) = row.manifest_toml.parse::<Manifest>() else {
            continue;
        };
        if other.slug.as_ref() == Some(slug) {
            return Err(bad_request(format!(
                "slug {slug:?} is already used by plugin {}; a slug is a URL and two \
                 plugins cannot share one",
                row.id
            )));
        }
    }
    Ok(())
}

/// Install a plugin, or upgrade one already installed.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InstallPluginRequest {
    /// The manifest, as TOML. Stored verbatim: it is the exact text the
    /// install gate accepted, and re-parsing it on boot runs the same check
    /// rather than trusting a second representation.
    pub manifest_toml: String,
    /// The wasm module, base64 (standard alphabet, padded).
    pub wasm_base64: String,

    /// The plugin's own sqlx migrations, filename to SQL text.
    ///
    /// Empty for a plugin that keeps no state of its own. A plugin that does
    /// cannot get tables any other way: its schema is created here and these
    /// are the only statements ever run against it as an owner, because the
    /// plugin's own role is granted no `CREATE`.
    ///
    /// Filenames follow sqlx's convention (`20260918000000_init.sql`), and
    /// their **bytes** are the checksum sqlx records. Re-sending a file whose
    /// content changed after it has applied anywhere breaks that install and
    /// every future upgrade of it - add a new file instead.
    #[serde(default)]
    pub migrations: std::collections::BTreeMap<String, String>,
}

/// Why a plugin is being switched off.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DisablePluginRequest {
    /// Shown to whoever next looks at the plugin list. Optional, but an
    /// unexplained disabled plugin is a thing the next admin has to
    /// re-derive.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What a lifecycle change did.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginMutationResponse {
    pub id: String,
    /// What just happened: `installed`, `upgraded`, `enabled`, `disabled`,
    /// `uninstalled`.
    pub outcome: String,
    /// True when the change does not take full effect until the server is
    /// restarted. Always true for install, upgrade and enable, which need a
    /// boot to compile and instantiate the module; false for a disable that
    /// this process applied to a live plugin.
    pub restart_required: bool,
    /// Plain-language detail for an admin, including what has *not* happened
    /// yet.
    pub detail: String,
}

/// One row of the audit trail.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginEventInfo {
    pub event: String,
    pub version: Option<String>,
    pub detail: Option<String>,
    /// The admin who did it, or absent when the host did it to itself - a
    /// crash-disable or a failed load has no actor.
    pub actor_user_id: Option<String>,
    pub at: chrono::DateTime<Utc>,
}

/// The audit trail for one plugin.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginEventListResponse {
    pub plugin_id: String,
    pub events: Vec<PluginEventInfo>,
}

/// What asking a plugin to cancel a subscription came back with.
#[derive(Debug, Serialize, ToSchema)]
pub struct CancelSubscriptionResponse {
    pub plugin_id: String,
    pub account_id: String,
    /// False on a refusal the plugin explains in `detail`. A call that could
    /// not run at all is not this response - see the 502 response below.
    pub cancelled: bool,
    /// Plain-language detail: what happened, and on a refusal, why.
    pub detail: String,
}

// ============================================================================
// Helpers
// ============================================================================

fn bad_request(msg: impl Into<String>) -> ApiErr {
    ApiErr::from((StatusCode::BAD_REQUEST, msg.into()))
}

fn server_error(msg: impl Into<String>) -> ApiErr {
    ApiErr::from((StatusCode::INTERNAL_SERVER_ERROR, msg.into()))
}

/// Parse the id out of the path, refusing anything `PluginId` would not
/// accept.
///
/// This is the same validation that keeps a plugin id safe as a path segment
/// and as a directory name, so it has to run before the id reaches either -
/// see `payserver_plugin_api::PluginId`.
fn parse_id(raw: &str) -> Result<PluginId, ApiErr> {
    PluginId::new(raw).map_err(|e| bad_request(e.to_string()))
}

/// Create the plugin's schema, run its migrations, and give it a login role.
///
/// Returns the role's password to be stored with the install record, or
/// `None` when this instance has no pools - in which case the plugin is
/// installed without database access rather than with the host's connection.
///
/// Migrations are materialised to a temporary directory because sqlx's
/// `Migrator` reads a path. They are the plugin's own bytes, written and read
/// unchanged, so the checksum sqlx records is the one the author produced.
async fn provision_storage<A>(
    state: &PgAppState<A>,
    id: &PluginId,
    migrations: &std::collections::BTreeMap<String, String>,
) -> Result<Option<String>, ApiErr>
where
    A: SessionService + 'static,
{
    let Some(pools) = state.plugin_pools.as_ref() else {
        return Ok(None);
    };

    let dir = tempfile::tempdir()
        .map_err(|e| server_error(format!("could not stage the plugin's migrations: {e}")))?;
    for (name, sql) in migrations {
        // A filename is a path component and nothing else. Without this a
        // migration named `../../etc/thing` would be written outside the
        // staging directory.
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(bad_request(format!(
                "migration filename {name:?} must be a plain filename"
            )));
        }
        std::fs::write(dir.path().join(name), sql)
            .map_err(|e| server_error(format!("could not stage migration {name}: {e}")))?;
    }

    let storage = PluginStorage::new(state.data_service.pool().clone());

    // Run the migrations on a blocking thread rather than awaiting them here.
    // sqlx's filesystem `MigrationSource` resolves through a `BoxFuture` that
    // is not `Send`, so awaiting `install` directly makes this whole handler's
    // future non-`Send` and axum will not accept it. `Handle::block_on` has no
    // `Send` bound, and a blocking thread is where a directory walk plus a
    // series of DDL statements belongs anyway.
    let handle = tokio::runtime::Handle::current();
    let staged = dir.path().to_path_buf();
    let for_task = storage.clone();
    let for_id = id.clone();
    tokio::task::spawn_blocking(move || handle.block_on(for_task.install(&for_id, &staged)))
        .await
        .map_err(|e| server_error(format!("the migration task did not finish: {e}")))?
        .map_err(|e| bad_request(format!("the plugin's migrations did not apply: {e}")))?;

    let password = generate_role_password().map_err(|e| server_error(e.to_string()))?;
    storage
        .provision_role(id, &password)
        .await
        .map_err(|e| server_error(format!("could not provision the plugin's role: {e}")))?;

    // Eagerly, once: the pool itself is lazy, so without this a wrong
    // credential would not surface until the plugin's first call, long after
    // the admin who could fix it has moved on.
    pools
        .register(id, &password)
        .await
        .map_err(|e| server_error(format!("could not register the plugin's pool: {e}")))?;

    Ok(Some(password))
}

/// Close `id`'s pool and drop its login role.
///
/// Order matters: Postgres refuses to drop a role while a session is
/// authenticated as it, so closing the pool second fails half-way and leaves
/// the role behind - still granted on a schema whose plugin is gone.
///
/// The schema and its data are deliberately kept. An admin uninstalling a
/// plugin to debug it should not lose its records - the billing plugin's
/// subscriptions most of all - and a reinstall provisions a fresh role
/// against the same tables.
///
/// A failure here does not fail the uninstall. The plugin is already disabled
/// and its row gone; refusing over a leftover role would leave an admin with
/// a plugin they cannot finish removing.
async fn release_database_access<A>(state: &PgAppState<A>, id: &PluginId)
where
    A: SessionService + 'static,
{
    if let Some(pools) = state.plugin_pools.as_ref() {
        pools.remove(id).await;
    }
    if let Err(e) = PluginStorage::new(state.data_service.pool().clone())
        .drop_role(id)
        .await
    {
        tracing::error!(
            plugin_id = %id,
            error = %e,
            "uninstalled the plugin but could not drop its database role; it grants access to a \
             schema whose plugin is gone and should be removed by hand"
        );
    }
}

fn artifacts<A>(state: &PgAppState<A>) -> PluginArtifacts {
    PluginArtifacts::new(state.plugin_dir.clone())
}

// ============================================================================
// Handlers
// ============================================================================

/// List installed plugins, what the next boot will do with each, and what
/// the host is doing with each right now.
///
/// The database is the authority on what is installed - the host only knows
/// what it managed to load, so asking it alone would silently omit exactly
/// the plugins an admin opened this page to find.
#[utoipa::path(
    get,
    path = "/admin/plugins",
    tag = "admin",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Installed plugins", body = AdminPluginListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
    )
)]
pub async fn list_plugins<A>(
    AdminAuth(_admin): AdminAuth,
    State(state): State<PgAppState<A>>,
) -> Result<Json<AdminPluginListResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let installed = InstalledPluginReader::list_installed_plugins(&*state.data_service)
        .await
        .map_err(|e| server_error(format!("could not read installed plugins: {e}")))?;

    let plugins = installed
        .into_iter()
        .map(|row| {
            // A row whose id no longer parses cannot be looked up in the
            // host, but it is still installed and still the admin's to
            // remove - so it is listed as not loaded rather than hidden.
            let snapshot = PluginId::new(row.id.clone())
                .ok()
                .and_then(|id| state.plugin_host.as_ref().and_then(|h| h.status(&id)));

            AdminPluginInfo {
                id: row.id,
                version: row.version,
                enabled: row.enabled,
                loaded: snapshot.as_ref().is_some_and(|s| s.enabled),
                // The host's live reason wins over the stored one: if a
                // plugin was disabled after this boot started, the database
                // still says why it was disabled last time, which is the
                // wrong answer to "why is it off now".
                disabled_reason: snapshot
                    .as_ref()
                    .and_then(|s| s.disabled_reason.clone())
                    .or(row.disabled_reason),
                consecutive_failures: snapshot.as_ref().map_or(0, |s| s.consecutive_failures),
                installed_at: row.installed_at,
                updated_at: row.updated_at,
            }
        })
        .collect();

    Ok(Json(AdminPluginListResponse {
        plugins,
        safe_mode: state.safe_mode,
    }))
}

/// Install a plugin, or upgrade one already installed.
///
/// Everything is validated before anything is written. The manifest has to
/// parse, it has to pass the same registry gate a boot would apply, and the
/// module has to be accepted by wasmtime - so a plugin this host could never
/// load is refused here, with a reason, rather than accepted and then found
/// broken on the next restart by whoever happens to be on call.
///
/// The digest is computed from the uploaded bytes and never taken from the
/// caller: a digest supplied alongside the artifact it describes proves
/// nothing.
#[utoipa::path(
    post,
    path = "/admin/plugins",
    tag = "admin",
    request_body = InstallPluginRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Installed", body = PluginMutationResponse),
        (status = 400, description = "The manifest or the module was refused"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin access required"),
    )
)]
pub async fn install_plugin<A>(
    AdminAuth(admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Json(req): Json<InstallPluginRequest>,
) -> Result<Json<PluginMutationResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let manifest: Manifest = req
        .manifest_toml
        .parse()
        .map_err(|e| bad_request(format!("manifest does not parse: {e}")))?;

    let wasm = base64::engine::general_purpose::STANDARD
        .decode(req.wasm_base64.trim())
        .map_err(|e| bad_request(format!("wasm_base64 is not valid base64: {e}")))?;

    if wasm.is_empty() {
        return Err(bad_request("wasm_base64 decoded to nothing"));
    }
    if wasm.len() > MAX_WASM_BYTES {
        return Err(bad_request(format!(
            "wasm module is {} bytes, over the {MAX_WASM_BYTES} byte limit",
            wasm.len()
        )));
    }

    // The same gate the boot loader applies, run now so an incompatible
    // plugin is refused at the moment an admin can still do something about
    // it. A fresh registry rather than the live host's: this is asking "would
    // this manifest be accepted", not claiming the id.
    PluginRegistry::new(host_version())
        .register(manifest.clone())
        .map_err(|e| bad_request(e.to_string()))?;

    check_ui_schema(manifest.ui_schema).map_err(bad_request)?;

    let id = manifest.id.clone();
    let version = manifest.version.to_string();

    refuse_a_taken_slug(&state, &manifest, &id).await?;

    let existing = InstalledPluginReader::get_installed_plugin(&*state.data_service, id.as_str())
        .await
        .map_err(|e| server_error(format!("could not read the install record: {e}")))?;
    let upgrade = existing.is_some();

    let artifacts = artifacts(&state);
    let sha256 = artifacts
        .write(&id, &version, &wasm)
        .map_err(|e| server_error(e.to_string()))?;

    // Storage before the install record, deliberately. A plugin whose
    // migrations failed is not installed - recording it first would leave a
    // row pointing at a schema that does not have the tables the plugin
    // expects, which boots fine and fails on the first call.
    let db_role_password = provision_storage(&state, &id, &req.migrations).await?;

    let record = NewInstalledPlugin {
        id: id.as_str().to_string(),
        version: version.clone(),
        manifest_toml: req.manifest_toml,
        artifact_sha256: sha256,
        // `None` when this instance has no pools. The upsert COALESCEs, so
        // that preserves whatever credential the plugin already had rather
        // than clearing it - an upgrade must not strip a working plugin of
        // its database access.
        db_role_password,
    };
    InstalledPluginWriter::upsert_installed_plugin(&*state.data_service, &record)
        .await
        .map_err(|e| server_error(format!("could not record the install: {e}")))?;

    let kind = if upgrade {
        PluginEventKind::Upgraded
    } else {
        PluginEventKind::Installed
    };
    record_event(
        &state,
        id.as_str(),
        kind,
        Some(version.clone()),
        None,
        Some(admin.id.0),
    )
    .await;

    let outcome = if upgrade { "upgraded" } else { "installed" };
    tracing::info!(
        plugin_id = %id,
        version = %version,
        actor = %admin.id,
        "plugin {outcome}"
    );

    Ok(Json(PluginMutationResponse {
        id: id.as_str().to_string(),
        outcome: outcome.to_string(),
        restart_required: true,
        detail: format!(
            "{id} {version} is {outcome} and will load on the next restart. It is not \
             running yet: this server loads plugins at boot rather than swapping them in \
             underneath live requests."
        ),
    }))
}

/// Turn a plugin back on for future boots.
#[utoipa::path(
    post,
    path = "/admin/plugins/{id}/enable",
    tag = "admin",
    params(("id" = String, Path, description = "Plugin id")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Enabled", body = PluginMutationResponse),
        (status = 404, description = "No such plugin"),
    )
)]
pub async fn enable_plugin<A>(
    AdminAuth(admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Path(raw_id): Path<String>,
) -> Result<Json<PluginMutationResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let id = parse_id(&raw_id)?;
    let changed =
        InstalledPluginWriter::set_plugin_enabled(&*state.data_service, id.as_str(), true, None)
            .await
            .map_err(|e| server_error(format!("could not enable the plugin: {e}")))?;

    if !changed {
        return Err(ApiErr::from((
            StatusCode::NOT_FOUND,
            format!("{id} is not installed"),
        )));
    }

    // The mirror of disable. When this process already holds the plugin -
    // disabled earlier in this same boot, by an admin or by repeated failure
    // - the module is compiled and instantiated and there is nothing to
    // rebuild, so enabling takes effect immediately. Requiring a restart to
    // undo a disable that took none would make an accidental disable far more
    // expensive to reverse than it was to cause.
    let started_now = state
        .plugin_host
        .as_ref()
        .is_some_and(|host| host.enable(&id));

    record_event(
        &state,
        id.as_str(),
        PluginEventKind::Enabled,
        None,
        None,
        Some(admin.id.0),
    )
    .await;
    tracing::info!(plugin_id = %id, actor = %admin.id, started_now, "plugin enabled");

    let detail = if started_now {
        format!("{id} is running again.")
    } else {
        // Nothing loaded to switch back on: installed since this process
        // started, safe mode, or a boot that could not read its artifact.
        // Compiling a module into a live host is the hot-reload this design
        // deliberately does not do, so this one really does need a restart.
        format!("{id} is enabled and will load on the next restart.")
    };

    Ok(Json(PluginMutationResponse {
        id: id.as_str().to_string(),
        outcome: "enabled".to_string(),
        restart_required: !started_now,
        detail,
    }))
}

/// Switch a plugin off - now, and for future boots.
#[utoipa::path(
    post,
    path = "/admin/plugins/{id}/disable",
    tag = "admin",
    params(("id" = String, Path, description = "Plugin id")),
    request_body = DisablePluginRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Disabled", body = PluginMutationResponse),
        (status = 404, description = "No such plugin"),
    )
)]
pub async fn disable_plugin<A>(
    AdminAuth(admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Path(raw_id): Path<String>,
    Json(req): Json<DisablePluginRequest>,
) -> Result<Json<PluginMutationResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let id = parse_id(&raw_id)?;
    let reason = req
        .reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| format!("disabled by admin {}", admin.id));

    let changed = InstalledPluginWriter::set_plugin_enabled(
        &*state.data_service,
        id.as_str(),
        false,
        Some(&reason),
    )
    .await
    .map_err(|e| server_error(format!("could not disable the plugin: {e}")))?;

    if !changed {
        return Err(ApiErr::from((
            StatusCode::NOT_FOUND,
            format!("{id} is not installed"),
        )));
    }

    // The asymmetry with enable is the point. An admin disabling a plugin
    // means stop running it, not stop running it after I restart the server -
    // this is the first of the three documented ways back in from a plugin
    // that is misbehaving, and it would not be a way back in at all if it
    // only took effect on the next boot.
    let stopped_now = state
        .plugin_host
        .as_ref()
        .is_some_and(|host| host.disable(&id, reason.clone()));

    record_event(
        &state,
        id.as_str(),
        PluginEventKind::Disabled,
        None,
        Some(reason),
        Some(admin.id.0),
    )
    .await;
    tracing::warn!(plugin_id = %id, actor = %admin.id, stopped_now, "plugin disabled");

    let detail = if stopped_now {
        format!("{id} has stopped running and will not load on the next restart.")
    } else {
        // Nothing to stop: safe mode, a plugin installed since this process
        // started, or one that never loaded. Saying "stopped" would be false.
        format!("{id} was not loaded in this process; it will not load on the next restart.")
    };

    Ok(Json(PluginMutationResponse {
        id: id.as_str().to_string(),
        outcome: "disabled".to_string(),
        restart_required: false,
        detail,
    }))
}

/// Ask an installed plugin to cancel one account's subscription now.
///
/// This is the operator-side path onto a state nothing else can reach: a
/// plugin page is read-only, so a merchant cannot ask to stop being billed
/// through one, and there is no other trigger anywhere in this server for a
/// plugin write. An admin picking the plugin and the account explicitly is
/// deliberately not the same shape as the broadcast dispatch in
/// `services::plugins::dispatch` - see that module's doc for why a wrong
/// answer here costs one account, not every invoice on the instance.
///
/// The plugin owns whatever "cancelled" means for its own schema; this route
/// only carries the ask and the plugin's answer. A plugin that does not
/// implement `cancel_subscription` answers exactly like one that trapped -
/// [`StatusCode::BAD_GATEWAY`], not a silent no-op reported as success.
#[utoipa::path(
    post,
    path = "/admin/plugins/{id}/accounts/{account_id}/cancel-subscription",
    tag = "admin",
    params(
        ("id" = String, Path, description = "Plugin id"),
        ("account_id" = String, Path, description = "The account whose subscription to cancel"),
    ),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "The plugin answered", body = CancelSubscriptionResponse),
        (status = 404, description = "No such plugin, or plugins are disabled on this instance"),
        (status = 502, description = "The plugin could not run the call"),
    )
)]
pub async fn cancel_plugin_subscription<A>(
    AdminAuth(admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Path((raw_id, account_id)): Path<(String, String)>,
) -> Result<Json<CancelSubscriptionResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let id = parse_id(&raw_id)?;

    let host = state.plugin_host.as_ref().ok_or_else(|| {
        ApiErr::from((
            StatusCode::NOT_FOUND,
            format!("{id} is not loaded - plugins are disabled on this instance"),
        ))
    })?;

    let outcome = cancel_subscription(host, &id, &account_id).await;

    tracing::warn!(
        plugin_id = %id,
        %account_id,
        actor = %admin.id,
        outcome = ?outcome,
        "admin requested a subscription cancellation"
    );

    let (cancelled, detail) = match outcome {
        CancelSubscriptionOutcome::Cancelled => (
            true,
            format!("{id} cancelled the subscription for {account_id}."),
        ),
        CancelSubscriptionOutcome::Refused { reason } => (
            false,
            reason.unwrap_or_else(|| {
                format!("{id} declined to cancel the subscription for {account_id}.")
            }),
        ),
        CancelSubscriptionOutcome::CouldNotRun { reason } => {
            return Err(ApiErr::from((
                StatusCode::BAD_GATEWAY,
                format!("{id} could not run the cancellation: {reason}"),
            )));
        }
    };

    Ok(Json(CancelSubscriptionResponse {
        plugin_id: id.as_str().to_string(),
        account_id,
        cancelled,
        detail,
    }))
}

/// Remove a plugin: its install record and its artifact.
///
/// The plugin's Postgres schema is deliberately left alone. Uninstalling is
/// how an admin reacts to a plugin misbehaving, and destroying its data as a
/// side effect of switching it off would make that reaction unrecoverable -
/// a reinstall would come back empty with no warning that anything was lost.
/// Dropping the schema is a separate, explicit act, and does not exist yet.
#[utoipa::path(
    delete,
    path = "/admin/plugins/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Plugin id")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Uninstalled", body = PluginMutationResponse),
        (status = 404, description = "No such plugin"),
    )
)]
pub async fn uninstall_plugin<A>(
    AdminAuth(admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Path(raw_id): Path<String>,
) -> Result<Json<PluginMutationResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let id = parse_id(&raw_id)?;

    let row = InstalledPluginReader::get_installed_plugin(&*state.data_service, id.as_str())
        .await
        .map_err(|e| server_error(format!("could not read the install record: {e}")))?
        .ok_or_else(|| ApiErr::from((StatusCode::NOT_FOUND, format!("{id} is not installed"))))?;

    // Stop dispatching before removing the record, so there is no window in
    // which the plugin is uninstalled on paper and still being called.
    let stopped_now = state
        .plugin_host
        .as_ref()
        .is_some_and(|host| host.disable(&id, "uninstalled"));

    release_database_access(&state, &id).await;

    InstalledPluginWriter::remove_installed_plugin(&*state.data_service, id.as_str())
        .await
        .map_err(|e| server_error(format!("could not remove the install record: {e}")))?;

    // Artifact last, and a failure here does not fail the request: the
    // record is already gone, so the plugin will not load again either way,
    // and refusing the uninstall over a leftover file would leave an admin
    // unable to complete the one action they came to perform.
    let artifact_removed = match artifacts(&state).remove(&id, &row.version) {
        Ok(removed) => removed,
        Err(e) => {
            tracing::error!(
                plugin_id = %id,
                version = %row.version,
                error = %e,
                "uninstalled the plugin but could not remove its artifact; the file is \
                 orphaned and safe, but it is still on disk"
            );
            false
        }
    };

    record_event(
        &state,
        id.as_str(),
        PluginEventKind::Uninstalled,
        Some(row.version.clone()),
        Some(if artifact_removed {
            "record and artifact removed".to_string()
        } else {
            "record removed; artifact left on disk".to_string()
        }),
        Some(admin.id.0),
    )
    .await;
    tracing::warn!(
        plugin_id = %id,
        version = %row.version,
        actor = %admin.id,
        stopped_now,
        artifact_removed,
        "plugin uninstalled"
    );

    Ok(Json(PluginMutationResponse {
        id: id.as_str().to_string(),
        outcome: "uninstalled".to_string(),
        restart_required: false,
        detail: format!(
            "{id} is uninstalled. Its data schema was not dropped - reinstalling restores \
             it; removing it is a separate act."
        ),
    }))
}

/// The audit trail for one plugin, newest first.
///
/// Survives the plugin: `plugin_events` has no foreign key to
/// `installed_plugins`, so this still answers for something uninstalled -
/// which is usually when someone comes asking.
#[utoipa::path(
    get,
    path = "/admin/plugins/{id}/events",
    tag = "admin",
    params(("id" = String, Path, description = "Plugin id")),
    security(("bearer_auth" = [])),
    responses((status = 200, description = "Audit trail", body = PluginEventListResponse))
)]
pub async fn plugin_events<A>(
    AdminAuth(_admin): AdminAuth,
    State(state): State<PgAppState<A>>,
    Path(raw_id): Path<String>,
) -> Result<Json<PluginEventListResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let id = parse_id(&raw_id)?;
    let events = InstalledPluginReader::plugin_events(&*state.data_service, id.as_str(), 100)
        .await
        .map_err(|e| server_error(format!("could not read the plugin's events: {e}")))?;

    Ok(Json(PluginEventListResponse {
        plugin_id: id.as_str().to_string(),
        events: events
            .into_iter()
            .map(|e| PluginEventInfo {
                event: e.event,
                version: e.version,
                detail: e.detail,
                actor_user_id: e.actor_user_id.map(|u| u.to_string()),
                at: e.at,
            })
            .collect(),
    }))
}

/// Write an audit row, reporting rather than propagating a failure.
///
/// The lifecycle change has already happened by the time this runs. Failing
/// the request because the *record* of it could not be written would tell an
/// admin the action did not happen when it did, which is a worse lie than a
/// gap in the audit trail - and the gap is logged loudly.
async fn record_event<A>(
    state: &PgAppState<A>,
    plugin_id: &str,
    kind: PluginEventKind,
    version: Option<String>,
    detail: Option<String>,
    actor_user_id: Option<uuid::Uuid>,
) {
    let event = NewPluginEvent {
        plugin_id: plugin_id.to_string(),
        kind,
        version,
        detail,
        actor_user_id,
    };
    if let Err(e) = InstalledPluginWriter::record_plugin_event(&*state.data_service, &event).await {
        tracing::error!(
            plugin_id = %plugin_id,
            event = kind.as_str(),
            error = %e,
            "a plugin lifecycle change happened but could not be recorded in plugin_events"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn a_newer_page_schema_than_the_client_draws_is_refused() {
        assert!(check_ui_schema(None).is_ok());
        assert!(check_ui_schema(Some(SUPPORTED_UI_SCHEMA)).is_ok());
        let err = check_ui_schema(Some(SUPPORTED_UI_SCHEMA + 1)).unwrap_err();
        assert!(err.contains("page schema"), "{err}");
    }

    /// Through the handler, not the helper: deleting the `check_ui_schema`
    /// call from `install_plugin` must turn this red.
    ///
    /// The state's pool points at a host that does not exist and connects
    /// lazily, so any attempt to reach the database - the slug check, the
    /// install record, storage provisioning - would answer 500, not 400. A
    /// 400 carrying the schema message therefore also shows the refusal
    /// happens before anything is written, so nothing is left installed. The
    /// registry `register` runs against is a throwaway built in the handler,
    /// so it holds no state either.
    #[tokio::test]
    async fn install_refuses_a_page_schema_the_client_cannot_draw() {
        use axum::response::IntoResponse;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://plugin-install-test-unused/db")
            .expect("connect_lazy only validates the URL, it does not connect");
        let data_service = std::sync::Arc::new(data_service::PgDataService::new(pool));
        let auth_service = std::sync::Arc::new(auth::AuthService::with_config(
            std::sync::Arc::clone(&data_service),
            auth::AuthConfig::default(),
        ));
        let state = crate::state::AppState::new(
            data_service,
            auth_service,
            None,
            std::sync::Arc::new(NoRatesForRouting),
            std::sync::Arc::new(crate::services::email::NoopEmailSender),
        );
        let admin = AdminAuth(auth::UserInfo {
            id: auth::UserId(uuid::Uuid::new_v4()),
            email: None,
            primary_wallet_address: None,
            created_at: chrono::Utc::now(),
            last_login_at: None,
            role: auth::Role::ServerAdmin,
        });
        let manifest_toml = format!(
            "id = \"cash.random.uitest\"\nversion = \"0.1.0\"\n\
             dependencies = [\"ethpayserver:^0.1\"]\nkind = \"action\"\n\
             ui_schema = {}\n",
            SUPPORTED_UI_SCHEMA + 1
        );
        let req = InstallPluginRequest {
            manifest_toml,
            wasm_base64: base64::engine::general_purpose::STANDARD.encode(b"\0asm"),
            migrations: Default::default(),
        };

        let resp = match install_plugin(admin, State(state), Json(req)).await {
            Ok(_) => panic!("a manifest needing a newer page schema must not install"),
            Err(e) => e.into_response(),
        };
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("page schema"), "{body}");
    }

    /// Every plugin lifecycle route must be mounted on the router the server
    /// actually serves - not on one a test built to look like it.
    ///
    /// Three pieces of this repository have shipped fully tested and reachable
    /// from nothing, `api::plugins::router()` among them, with nine passing
    /// tests and no mount. So a handler with green unit tests is not evidence
    /// that a request can reach it.
    ///
    /// Asserts the `Allow` header rather than just "not 404". A wrong-method
    /// probe alone cannot tell a missing `POST /admin/plugins` from a present
    /// one, because `GET` is mounted on that same path and answers the probe
    /// with 405 either way - so dropping the install route would have left
    /// this green. `Allow` names every method the path actually serves.
    #[tokio::test]
    async fn every_plugin_route_is_mounted_on_the_real_router() {
        // (path, a method the route does not serve, the methods it must)
        let probes = [
            ("/admin/plugins", "PUT", vec!["GET", "POST"]),
            ("/admin/plugins/cash.random.billing", "PUT", vec!["DELETE"]),
            (
                "/admin/plugins/cash.random.billing/enable",
                "GET",
                vec!["POST"],
            ),
            (
                "/admin/plugins/cash.random.billing/disable",
                "GET",
                vec!["POST"],
            ),
            (
                "/admin/plugins/cash.random.billing/events",
                "POST",
                vec!["GET"],
            ),
            (
                "/admin/plugins/cash.random.billing/accounts/acct-1/cancel-subscription",
                "GET",
                vec!["POST"],
            ),
        ];

        for (path, wrong_method, expected) in probes {
            let resp = router_under_test()
                .oneshot(
                    Request::builder()
                        .method(wrong_method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} is not mounted on the production router; an admin cannot reach a \
                 handler nothing routes to"
            );
            assert_eq!(
                resp.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{path} should exist but not serve {wrong_method}"
            );

            let allow = resp
                .headers()
                .get(axum::http::header::ALLOW)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            for method in expected {
                assert!(
                    allow.contains(method),
                    "{path} should serve {method}, but Allow is {allow:?}"
                );
            }
        }
    }

    /// And each one is behind admin auth, answering 401 rather than acting.
    ///
    /// Probed with the real method this time: a route that is mounted but
    /// unauthenticated would pass the test above and still let anyone install
    /// code into the server.
    #[tokio::test]
    async fn every_plugin_route_refuses_an_unauthenticated_caller() {
        let probes = [
            ("GET", "/admin/plugins"),
            ("POST", "/admin/plugins"),
            ("DELETE", "/admin/plugins/cash.random.billing"),
            ("POST", "/admin/plugins/cash.random.billing/enable"),
            ("POST", "/admin/plugins/cash.random.billing/disable"),
            ("GET", "/admin/plugins/cash.random.billing/events"),
            (
                "POST",
                "/admin/plugins/cash.random.billing/accounts/acct-1/cancel-subscription",
            ),
        ];

        for (method, path) in probes {
            let resp = router_under_test()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path} must refuse an anonymous caller before doing anything"
            );
        }
    }

    /// The production router, built from the same function `bin/server.rs`
    /// calls - not a mirror of it assembled here, which could agree with
    /// itself while the served router has no such path.
    fn router_under_test() -> axum::Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://plugin-route-test-unused/db")
            .expect("connect_lazy only validates the URL, it does not connect");
        let data_service = std::sync::Arc::new(data_service::PgDataService::new(pool));
        let auth_service = std::sync::Arc::new(auth::AuthService::with_config(
            std::sync::Arc::clone(&data_service),
            auth::AuthConfig::default(),
        ));
        let state = crate::state::AppState::new(
            data_service,
            auth_service,
            None,
            std::sync::Arc::new(NoRatesForRouting),
            std::sync::Arc::new(crate::services::email::NoopEmailSender),
        );
        crate::api::router(state, false, None, None, None)
    }

    /// The router needs a rate provider; these probes never reach one,
    /// because every request is refused on method or on auth first.
    struct NoRatesForRouting;

    #[async_trait::async_trait]
    impl rates::RateProvider for NoRatesForRouting {
        async fn get_rate(
            &self,
            _from: &str,
            _to: &str,
        ) -> Result<rates::ExchangeRate, rates::RateError> {
            unreachable!("refused on method or auth before any handler runs")
        }

        fn name(&self) -> &'static str {
            "none"
        }
    }
}
