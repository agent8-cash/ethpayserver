//! ETHPayServer core - unified API and service orchestration.
//!
//! This crate provides the main server binary that combines all ethpayserver
//! functionality into a single service.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     ETHPayServer Core                       │
//! ├─────────────────────────────────────────────────────────────┤
//! │  API Layer (axum)                                           │
//! │  ├── /health     - Health checks                            │
//! │  ├── /evm        - EVM operations (tokens, networks)        │
//! │  ├── /auth       - Authentication (TODO)                    │
//! │  └── /swagger-ui - API documentation                        │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Service Layer                                              │
//! │  ├── AuthService   - User authentication & sessions         │
//! │  └── (PaymentService, InvoiceService - TODO)                │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Data Layer                                                 │
//! │  └── PgDataService - PostgreSQL repositories                │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```bash
//! # Set required environment variables
//! export DATABASE_URL="postgres://user:pass@localhost/ethpayserver"
//!
//! # Run the server
//! cargo run --release
//! ```

pub mod api;
pub mod config;
pub mod metrics;
pub mod services;
pub mod state;
pub mod tracing_init;

pub use config::Config;
pub use services::{
    ArtifactError, ChainHealthMetricsConfig, ChainHealthMetricsService, CleanupConfig,
    CleanupError, CleanupStats, DEFAULT_CALL_DEADLINE, DEFAULT_MAX_FAILURES, DEFAULT_MAX_IN_FLIGHT,
    EVMMonitor, EVMMonitorError, EventConsumer, EventConsumerError, FilterOutcome,
    InvoiceCleanupService, PageElement, PageError, PageHost, PageRenderer, PluginArtifacts,
    PluginBootReport, PluginCallError, PluginEngine, PluginHost, PluginHostError, PluginInstance,
    PluginLoadError, PluginPools, PluginRegistry, PluginSchema, PluginStatusSnapshot,
    PluginStorage, PluginStorageError, PluginWasmError, RedisEVMMonitor, Viewer, WatchRetryConfig,
    WatchRetryService, WebhookConfig, WebhookOutboxDrainConfig, WebhookOutboxDrainService,
    WebhookService, WebhookSink, account_closed_observers, host_version, invoice_creation_filters,
    load_installed_plugins, own_store_payment_reporting, payment_observers, report_boot,
    store_creation_filters,
};
pub use state::{AppDataService, AppDataServiceReader, AppState};

// Re-export rates crate types for convenience
pub use rates::{
    CachedRateProvider, CoinGeckoRateProvider, ExchangeRate, FIAT_CURRENCIES, FallbackRateProvider,
    KrakenRateProvider, RateError, RateProvider, RateProviderConfig, is_crypto_currency,
    is_fiat_currency,
};
