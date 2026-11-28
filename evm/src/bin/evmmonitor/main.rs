//! EVM Monitor binary - monitors multiple EVM chains for payments.
//!
//! This binary can be run as a standalone service that monitors blockchain
//! transactions and publishes events to a Redis bridge. Multiple instances
//! can run in parallel, each handling a subset of chains.
//!
//! ## Bidirectional Communication
//!
//! The monitor supports bidirectional communication via Redis:
//!
//! - **Events** (monitor -> API server): PaymentDetected, PaymentConfirmed, etc.
//! - **Commands** (API server -> monitor): WatchAddress, UnwatchAddress, GetStatus
//!
//! This allows the API server to dynamically add/remove watched addresses without
//! restarting the monitor.
//!
//! # Configuration
//!
//! Configure via TOML file or environment variables:
//!
//! ```toml
//! # evmmonitor.toml
//! [bridge]
//! redis_url = "redis://localhost:6379"
//! events_channel = "evmmonitor:events"
//! commands_channel = "evmmonitor:commands"
//!
//! [[chains]]
//! chain_id = 1
//! rpc_http = "https://eth.llamarpc.com"
//! rpc_ws = "wss://eth.llamarpc.com"
//!
//! [[chains]]
//! chain_id = 137
//! rpc_http = "https://polygon-rpc.com"
//! ```
//!
//! Or via environment:
//! ```bash
//! EVMMONITOR_REDIS_URL=redis://localhost:6379
//! EVMMONITOR_CHAINS=1,137
//! EVMMONITOR_CHAIN_1_RPC_HTTP=https://eth.llamarpc.com
//! EVMMONITOR_CHAIN_1_RPC_WS=wss://eth.llamarpc.com
//! EVMMONITOR_CHAIN_137_RPC_HTTP=https://polygon-rpc.com
//! ```

mod chain;
mod commands;
mod config;
mod health;

use std::borrow::Cow;
use std::sync::Arc;

use clap::Parser;
use data_service::RedisDataService;
use evm::error::EvmResult;
use evm::monitor::bridge::{EventBridge, RedisBridge};
use evm::monitor::startup::{STARTUP_ATTEMPTS, STARTUP_INITIAL_DELAY, start_monitors};
use evm::monitor::{
    CoordinatorConfig, EventHandler, LoggingHandler, MonitorCoordinator, MonitorEvent,
};
use secrecy::ExposeSecret;
use tokio::signal;
use tracing::info;
use tracing_subscriber::{EnvFilter, util::SubscriberInitExt};

use chain::create_chain_monitor;
use commands::{handle_commands, restore_watched_addresses};
use config::{Args, get_chain_configs, load_config};
use health::publish_health_loop;

/// Handler that publishes events to the Redis bridge.
struct BridgeHandler {
    bridge: Arc<dyn EventBridge>,
}

impl BridgeHandler {
    fn new(bridge: Arc<dyn EventBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl EventHandler for BridgeHandler {
    async fn handle(&self, event: &MonitorEvent) -> EvmResult<()> {
        self.bridge.publish(event).await
    }

    fn name(&self) -> &str {
        "BridgeHandler"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file if present
    let _ = dotenvy::dotenv();

    // Initialize Sentry (no-op when SENTRY_DSN is unset). SENTRY_RELEASE is
    // set by the CI build step from GITHUB_SHA — option_env! reads it at
    // compile time, so it must be a real env var at `cargo build`, not
    // something exported at deploy/run time.
    let (_sentry_guard, sentry_dsn_configured, sentry_environment) =
        evm::telemetry::init_sentry(option_env!("SENTRY_RELEASE").map(Cow::from));

    // Parse CLI args
    let args = Args::parse();

    // Initialize logging (includes Sentry layer when DSN is configured)
    init_logging(&args.log_format, &args.log_level)?;

    info!("starting evmmonitor");

    // Report whether error reporting is actually on. Must come after
    // `init_logging`: `info!`/`error!` before that has no subscriber to write
    // to. This is the component that failed silently for 10.5 hours, so it
    // must not also be silently unreported.
    evm::telemetry::report_reporting_status(sentry_dsn_configured, &sentry_environment)?;

    // Install the Prometheus recorder before any RPC call runs, so
    // `evm::metrics::record_rpc_ok`/`record_rpc_err` (called from every
    // chain RPC this process makes) land somewhere instead of silently
    // no-op'ing against an uninstalled global recorder.
    evm::metrics::monitor_recorder::init()?;

    // A bind failure here must not take payment monitoring down with it:
    // this module's own doc (top of file) says multiple evmmonitor
    // instances can run in parallel on one host, each on a subset of
    // chains, and `server`'s own default METRICS_PORT is the same 9102 -
    // so without separate network namespaces, two processes racing for the
    // same loopback port is a real way to lose this purely observational
    // feature. Log and continue instead of `?`.
    let metrics_bind_addr = format!("{}:{}", args.metrics_host, args.metrics_port);
    let metrics_handle = match tokio::net::TcpListener::bind(&metrics_bind_addr).await {
        Ok(metrics_listener) => {
            info!("metrics listener bound to http://{}", metrics_bind_addr);
            Some(tokio::spawn(async move {
                let service = evm::metrics::monitor_recorder::router().into_make_service();
                if let Err(e) = axum::serve(metrics_listener, service).await {
                    tracing::error!(error = %e, "metrics listener error");
                }
            }))
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                addr = %metrics_bind_addr,
                "failed to bind metrics listener - continuing without /metrics"
            );
            None
        }
    };

    // Load configuration
    let config = load_config(&args)?;

    // Merge CLI args with config file
    let redis_url = args
        .redis_url
        .clone()
        .or(config.bridge.redis_url.clone())
        .ok_or_else(|| anyhow::anyhow!("redis_url is required"))?;
    // One explicit expose for the bridge and health publisher below, both of
    // which need an owned 'static value. The URL stays a secret where it is
    // stored — the CLI args and the config-file struct, which both derive Debug.
    let redis_url = redis_url.expose_secret().to_string();

    let events_channel = config
        .bridge
        .events_channel
        .clone()
        .unwrap_or_else(|| args.events_channel.clone());
    let commands_channel = config
        .bridge
        .commands_channel
        .clone()
        .unwrap_or_else(|| args.commands_channel.clone());

    // Get chain configs - merge CLI chains with config file
    let chain_configs = get_chain_configs(&args.chains, &config.chains)?;

    if chain_configs.is_empty() {
        anyhow::bail!("no chains configured");
    }

    info!(
        chains = ?chain_configs.iter().map(|c| c.chain_id).collect::<Vec<_>>(),
        "monitoring chains"
    );

    // Create Redis bridge with bidirectional channels
    let bridge = Arc::new(RedisBridge::new(&redis_url, &events_channel, &commands_channel).await?);
    info!(
        url = %redis_url,
        events = %events_channel,
        commands = %commands_channel,
        "connected to redis"
    );

    // Health check
    bridge.health_check().await?;
    info!("redis bridge health check passed");

    // Create Redis persistence service
    let persistence = Arc::new(RedisDataService::new(&redis_url).await?);
    persistence.health_check().await?;
    info!("redis persistence connected");

    // Create coordinator
    let coordinator = Arc::new(MonitorCoordinator::new(CoordinatorConfig::new()));

    // Register handlers
    coordinator
        .register_handler(Arc::new(LoggingHandler::new()))
        .await;
    coordinator
        .register_handler(Arc::new(BridgeHandler::new(bridge.clone())))
        .await;

    // Add chain monitors
    let monitored_chain_ids: Vec<u64> = chain_configs.iter().map(|c| c.chain_id).collect();
    // A monitor that failed to build is not a degraded mode: the process would
    // stay up, report nothing, and detect no payments on that chain. Retry
    // briefly for a transient RPC failure, then exit non-zero so the
    // supervisor restarts us and the failure is visible.
    start_monitors(
        &monitored_chain_ids,
        STARTUP_ATTEMPTS,
        STARTUP_INITIAL_DELAY,
        |chain_id| {
            let chain_config = chain_configs
                .iter()
                .find(|c| c.chain_id == chain_id)
                .expect("chain id comes from chain_configs");
            create_chain_monitor(chain_config)
        },
        |chain_id, monitor| {
            let coordinator = coordinator.clone();
            async move {
                coordinator.add_chain(monitor).await?;
                info!(chain_id, "chain monitor started");
                Ok(())
            }
        },
    )
    .await?;

    // Restore watched addresses from Redis persistence
    restore_watched_addresses(&coordinator, &persistence, &monitored_chain_ids).await;

    // Start coordinator
    coordinator.clone().start().await?;
    info!("monitor coordinator started");

    // Subscribe to commands from API server
    let commands_stream = bridge.subscribe_commands().await?;
    info!("subscribed to commands channel");

    // Spawn command handler task
    let command_coordinator = coordinator.clone();
    let command_bridge = bridge.clone();
    let command_persistence = persistence.clone();
    let mut command_handle = tokio::spawn(async move {
        handle_commands(
            commands_stream,
            command_coordinator,
            command_bridge,
            command_persistence,
        )
        .await;
    });

    // Spawn health publisher task
    let health_coordinator = coordinator.clone();
    let health_redis_url = redis_url.clone();
    let health_handle = tokio::spawn(async move {
        publish_health_loop(
            &health_coordinator,
            &health_redis_url,
            option_env!("SENTRY_RELEASE").unwrap_or_default(),
        )
        .await;
    });

    // Wait for shutdown signal
    shutdown_signal().await;
    info!("shutdown signal received");

    // Mark the bridge as shutting down before anything else, so a
    // subscription that ends while we tear down (the compose network can
    // drop out from under a still-running container) logs as expected
    // rather than as a fault.
    bridge.begin_shutdown();

    // Give the command subscription a moment to notice its connection ending
    // on its own and log itself as a shutdown before we forcibly cancel it.
    // Aborting immediately would race the stream's own end-of-stream tail:
    // `abort()` only takes effect on the task's next poll, so if the task
    // isn't already mid-poll when we call it, the task is dropped before
    // that tail (and its shutdown-vs-fault log line) ever runs.
    match tokio::time::timeout(std::time::Duration::from_secs(1), &mut command_handle).await {
        Err(_) => {
            tracing::debug!(
                "command handler task did not exit within the shutdown grace period, aborting"
            );
            command_handle.abort();
        }
        Ok(Err(join_error)) => {
            tracing::error!(error = %join_error, "command handler task ended unexpectedly during shutdown");
        }
        Ok(Ok(())) => {}
    }
    health_handle.abort();
    if let Some(handle) = metrics_handle {
        handle.abort();
    }

    // No equivalent handle exists here for the *events* subscription
    // (`redis.rs`'s `subscribe`, as opposed to `subscribe_commands` above):
    // evmmonitor never consumes that stream, only publishes to it via
    // `BridgeHandler`. Its one consumer is the server's `EventConsumer`,
    // which gets this same begin_shutdown-then-bounded-wait-then-abort
    // treatment for its own handle in `server/src/bin/server.rs` before that
    // process exits.

    // Graceful shutdown
    coordinator.stop().await?;
    info!("evmmonitor stopped");

    Ok(())
}

fn init_logging(format: &str, level: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(level)?;

    // Gates which levels become Sentry *structured logs* specifically, so
    // testnet can ship INFO there while mainnet ships WARN and above. Applied
    // as the Sentry layer's own per-layer filter (below) rather than folded
    // into `filter`, because a bare `.with(filter)` layer sits in the same
    // `Layered` stack as every other layer and `Layered::enabled` ANDs across
    // all of them — an event `filter` (LOG_LEVEL) rejects never reaches the
    // Sentry layer's `on_event` at all, so `SENTRY_LOG_LEVEL` could only ever
    // be a *further* restriction on top of LOG_LEVEL, never independent of
    // it. Per-layer filtering (`.with_filter` on each layer instead of a
    // shared `.with(filter)`) is what actually decouples them.
    let sentry_log_level = evm::telemetry::resolve_sentry_log_level();

    evm::telemetry::build_subscriber(filter, format == "json", sentry_log_level).init();

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
