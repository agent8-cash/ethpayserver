//! Application state shared across all handlers.

use std::sync::Arc;

use async_trait::async_trait;
use auth::StoreRoleRepository;
use data_service::{
    InvoiceReader, InvoiceWriter, PaymentReader, PaymentWriter, PayoutReader, PayoutWriter,
    RefundReader, RefundWriter, StoreWebhookReader, TokenReader, TokenWriter, WalletReader,
    WalletWriter, WatchedAddressReader, WatchedAddressWriter, WebhookDeliveryReader,
};
use evm::api::EvmDataService;
use rates::RateProvider;

use crate::api::ws::WsBroadcast;
use crate::services::email::EmailSender;
use crate::services::plugins::AccountClosedObserver;
use crate::services::plugins::InvoiceCreationFilter;
use crate::services::plugins::PageHost;
use crate::services::plugins::PluginHost;
use crate::services::webhook::WebhookSink;

/// Read-only data service trait for the application.
///
/// Use this bound for handlers that only read from the database.
#[async_trait]
pub trait AppDataServiceReader:
    InvoiceReader
    + PaymentReader
    + TokenReader
    + WatchedAddressReader
    + WalletReader
    + StoreWebhookReader
    + StoreRoleRepository
    + RefundReader
    + PayoutReader
    + WebhookDeliveryReader
    + Send
    + Sync
{
    /// Check database health.
    async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Full data service trait for the application (read + write).
///
/// Use this bound for handlers that modify the database.
#[async_trait]
pub trait AppDataService:
    AppDataServiceReader
    + InvoiceWriter
    + PaymentWriter
    + TokenWriter
    + WatchedAddressWriter
    + WalletWriter
    + RefundWriter
    + PayoutWriter
    + EvmDataService
{
}

/// Shared application state for all API handlers.
///
/// Generic over the data service type `D`, auth service type `A`,
/// and EVM monitor type `E`. `D` and `E` bounds are placed on handlers
/// to allow read-only vs read-write separation.
pub struct AppState<D, A, E> {
    /// Data service for database operations.
    pub data_service: Arc<D>,

    /// Authentication service.
    pub auth_service: Arc<A>,

    /// EVM monitor for sending commands to evmmonitor.
    /// None if Redis is not configured.
    pub evm_monitor: Option<Arc<E>>,

    /// Rate provider for fiat-to-crypto conversions.
    pub rate_provider: Arc<dyn RateProvider>,

    /// WebSocket broadcast channel for real-time status updates.
    pub ws_broadcast: Option<Arc<WsBroadcast>>,

    /// Optional CAPTCHA provider for registration endpoints.
    pub captcha_provider: Option<Arc<dyn auth::captcha::CaptchaProvider>>,

    /// Queue for webhook notifications emitted by HTTP handlers.
    ///
    /// The background services own their own handle to the same service; this
    /// one exists because some events are caused by a request, not by a chain
    /// event - `invoice_cancelled` is emitted by the cancel endpoint. None
    /// when Redis is not configured, in which case those events are dropped
    /// rather than queued.
    pub webhook_sink: Option<Arc<dyn WebhookSink>>,

    /// The WebAuthn relying party this process resolved at startup.
    ///
    /// Copied from the resolved `AuthConfig` *after* the explicit-or-derived
    /// rp_id logic has run and before that config is moved into `AuthService`,
    /// which keeps it private. Deliberately not re-read from the environment on
    /// request: the environment is the thing being verified, and reading it back
    /// would confirm only that a variable was set, not that it took effect.
    pub webauthn: Option<api_types::WebAuthnHealth>,

    /// Plugins that may refuse invoice creation (host capability 2), e.g.
    /// to enforce a lapsed subscription. Empty when no such plugin is
    /// installed, in which case invoice creation is never filtered at all.
    pub invoice_creation_filters: Vec<Arc<dyn InvoiceCreationFilter>>,

    /// Plugins told when an account is deleted (host capability 8). Empty
    /// when no plugin is installed, in which case a deleted account's rows in
    /// a plugin's own schema simply outlive it - nothing else is watching for
    /// this today.
    pub account_closed_observers: Vec<Arc<dyn AccountClosedObserver>>,

    /// The operator's own store: where this instance issues its own
    /// invoices, and the one store whose payments are reported to plugins
    /// through the own-store payment observers.
    ///
    /// It is NOT an exemption from `invoice_creation_filters`. That exemption
    /// is a property of the credential (`AuthenticatedCaller::is_operator`),
    /// so a store id settable through the admin settings API cannot widen it.
    /// Without an exemption of some kind a plugin that refuses - through a
    /// bug, or because it is down and fails closed - would refuse the renewal
    /// invoice that would have cleared the refusal.
    ///
    /// `None` on any instance that issues no invoices to itself, which
    /// reports nothing.
    pub operator_store_id: Option<types::StoreId>,

    /// The account `operator_store_id` must be owned by, per `Config`.
    ///
    /// Carried on the state the same way `operator_store_id` is: resolved
    /// once at boot and never re-read from the environment, since an admin
    /// settings save must be checked against the value this process actually
    /// started with, not a fresh guess at what the environment currently
    /// says.
    pub operator_account_id: Option<types::UserId>,
    /// Sender used to verify a pending email-address change.
    ///
    /// Unlike `webhook_sink` this is never `None`: `create_email_sender`
    /// always returns something, real or a no-op, and the email-change
    /// handler tells the two apart via `EmailSender::is_configured` rather
    /// than by the field's presence - it must fail loudly on a no-op sender,
    /// where a receipt would rather stay silent.
    pub email_sender: Arc<dyn EmailSender>,
    /// The plugin page host for `GET /plugins/{id}/pages/{path}`.
    ///
    /// Starts empty and stays empty in production until a wasmtime runtime
    /// exists to register a real [`PageRenderer`](crate::services::plugins::PageRenderer) -
    /// see that module's docs. Every request 404s until then, which is
    /// correct: there is no plugin code to invoke yet.
    pub plugin_pages: Arc<PageHost>,
    /// The wasmtime plugin host, once a boot has built one.
    ///
    /// `None` outside a real server: `PluginHost::new` builds a wasmtime
    /// engine and spawns its epoch ticker thread, which is the wrong price
    /// for the many unit tests that construct an `AppState` and never touch
    /// a plugin. The live server sets it in `server.rs` after
    /// `load_installed_plugins` has populated it, so `Some` here means the
    /// host exists and has already been told what is installed.
    ///
    /// It is also `None` in safe mode - that boot builds no host at all,
    /// which is what makes safe mode a property of the process rather than
    /// a flag every call site has to remember to check.
    pub plugin_host: Option<Arc<PluginHost>>,
    /// Where installed plugins' wasm artifacts live.
    ///
    /// Carried on the state rather than re-read from the environment per
    /// request, the same way `webauthn` and `safe_mode` are: the resolved
    /// value is what the boot loader actually used, and reading the variable
    /// back later would confirm only that it was set, not that this process
    /// agreed with it.
    pub plugin_dir: std::path::PathBuf,

    /// One connection pool per installed plugin, sharing one instance-wide
    /// budget. `None` on a boot that built no pools - safe mode, or a process
    /// that never reached the plugin stage - in which case a plugin gets no
    /// database access rather than the host's own connection.
    pub plugin_pools: Option<Arc<crate::services::plugins::PluginPools>>,
    /// Safe mode: this boot has every plugin disabled.
    ///
    /// Resolved once at startup from `Config::safe_mode` and copied in here,
    /// the same way `webauthn` is - not re-read from the environment on
    /// request, since the environment is the thing being reported on.
    pub safe_mode: bool,
}

// Manual Clone impl since we only need Arc::clone
impl<D, A, E> Clone for AppState<D, A, E> {
    fn clone(&self) -> Self {
        Self {
            data_service: Arc::clone(&self.data_service),
            auth_service: Arc::clone(&self.auth_service),
            evm_monitor: self.evm_monitor.clone(),
            rate_provider: Arc::clone(&self.rate_provider),
            ws_broadcast: self.ws_broadcast.clone(),
            captcha_provider: self.captcha_provider.clone(),
            webhook_sink: self.webhook_sink.clone(),
            webauthn: self.webauthn.clone(),
            invoice_creation_filters: self.invoice_creation_filters.clone(),
            account_closed_observers: self.account_closed_observers.clone(),
            operator_store_id: self.operator_store_id,
            operator_account_id: self.operator_account_id,
            email_sender: Arc::clone(&self.email_sender),
            plugin_pages: Arc::clone(&self.plugin_pages),
            plugin_host: self.plugin_host.clone(),
            plugin_dir: self.plugin_dir.clone(),
            plugin_pools: self.plugin_pools.clone(),
            safe_mode: self.safe_mode,
        }
    }
}

impl<D, A, E> AppState<D, A, E> {
    /// Create a new application state.
    pub fn new(
        data_service: Arc<D>,
        auth_service: Arc<A>,
        evm_monitor: Option<Arc<E>>,
        rate_provider: Arc<dyn RateProvider>,
        email_sender: Arc<dyn EmailSender>,
    ) -> Self {
        Self {
            data_service,
            auth_service,
            evm_monitor,
            rate_provider,
            ws_broadcast: None,
            captcha_provider: None,
            webhook_sink: None,
            webauthn: None,
            invoice_creation_filters: Vec::new(),
            account_closed_observers: Vec::new(),
            operator_store_id: None,
            operator_account_id: None,
            email_sender,
            plugin_pages: Arc::new(PageHost::new()),
            plugin_host: None,
            plugin_dir: std::path::PathBuf::from("./plugins"),
            plugin_pools: None,
            safe_mode: false,
        }
    }
}

impl<D: EvmDataService, A, E> AppState<D, A, E> {
    /// Convert to EVM API state.
    pub fn to_evm_state(&self) -> evm::api::EvmState<D, A> {
        evm::api::EvmState::new(
            Arc::clone(&self.data_service),
            Arc::clone(&self.auth_service),
        )
    }
}

// Implement AppDataServiceReader for PgDataService
#[async_trait]
impl AppDataServiceReader for data_service::PgDataService {
    async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.health_check()
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

// Implement AppDataService for PgDataService (marker trait, extends reader)
impl AppDataService for data_service::PgDataService {}

/// Convenient type alias for AppState with PgDataService and RedisEVMMonitor.
///
/// Use this in handlers to avoid specifying the full generic types:
/// ```rust,ignore
/// async fn handler(State(state): State<PgAppState<A>>) -> ...
/// ```
pub type PgAppState<A> = AppState<data_service::PgDataService, A, crate::services::RedisEVMMonitor>;
