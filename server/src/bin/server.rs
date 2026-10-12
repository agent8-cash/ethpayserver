//! ETHPayServer main binary.
//!
//! Start the server with:
//! ```bash
//! DATABASE_URL="postgres://..." cargo run --release
//! ```

use secrecy::ExposeSecret;
use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::signal;

use auth::{AuthConfig, AuthService, captcha::CloudflareTurnstile};
use data_service::PgDataService;
use evm::monitor::bridge::{COMMANDS_CHANNEL, EVENTS_CHANNEL, RedisBridge};
use rates::RateProviderConfig;
use server::{
    AppState, ChainHealthMetricsConfig, ChainHealthMetricsService, CleanupConfig, EventConsumer,
    InvoiceCleanupService, RedisEVMMonitor, WatchRetryConfig, WatchRetryService, WebhookConfig,
    WebhookService, api,
    api::api_key_rate_limit::ApiKeyRateLimitState,
    api::rate_limit::{RateLimitConfig, RateLimitState},
    config::Config,
    metrics,
};
use server::{
    DEFAULT_CALL_DEADLINE, DEFAULT_MAX_FAILURES, DEFAULT_MAX_IN_FLIGHT, PluginArtifacts,
    PluginHost, PluginPools, account_closed_observers, host_version, invoice_creation_filters,
    load_installed_plugins, own_store_payment_reporting, payment_observers, report_boot,
};

use server::tracing_init::init_tracing;

#[tokio::main]
#[allow(clippy::too_many_lines)] // server bootstrap — config, DB, services, routes in sequence
async fn main() -> Result<()> {
    // Load .env file if present
    let _ = dotenvy::dotenv();

    // Initialize Sentry (no-op when SENTRY_DSN is unset). SENTRY_RELEASE is
    // set by the CI build step from GITHUB_SHA — option_env! reads it at
    // compile time, so it must be a real env var at `cargo build`, not
    // something exported at deploy/run time.
    let (_sentry_guard, sentry_dsn_configured, sentry_environment) =
        evm::telemetry::init_sentry(option_env!("SENTRY_RELEASE").map(Cow::from));

    // Load configuration
    let config = Config::from_env()?;

    // Initialize tracing (includes Sentry layer when DSN is configured)
    init_tracing(&config.log_level, &config.log_format);

    // Report whether error reporting is actually on. `tracing::info!` before
    // this point has no subscriber to write to, so this must come after
    // `init_tracing`, not next to `init_sentry`.
    evm::telemetry::report_reporting_status(sentry_dsn_configured, &sentry_environment)?;

    // Initialize Prometheus metrics
    metrics::init_metrics()?;

    tracing::info!("Starting ETHPayServer v{}", env!("CARGO_PKG_VERSION"));

    if config.safe_mode {
        tracing::warn!(
            "SAFE MODE: ETHPAY_DISABLE_PLUGINS is set - every plugin (including billing) is \
             disabled for this boot. Plugins are not uninstalled and their data is untouched; \
             clear the flag and restart to bring them back."
        );
    }

    // Connect to database
    tracing::info!("Connecting to database...");
    let data_service = Arc::new(PgDataService::connect(config.database_url.expose_secret()).await?);
    tracing::info!("Database connected");

    // Create auth service with custom config
    // Note: PgDataService implements AuthRepository (all required traits)
    let mut auth_config = AuthConfig {
        // Increase wallet challenge timeout to 10 minutes for registration flow
        wallet_challenge_duration: chrono::Duration::minutes(10),
        ..AuthConfig::default()
    };

    // WebAuthn config from environment
    let rp_id_explicit = std::env::var("WEBAUTHN_RP_ID").ok();
    if let Ok(rp_origin) = std::env::var("WEBAUTHN_RP_ORIGIN") {
        auth_config.rp_origin = rp_origin;
    }
    if let Ok(rp_name) = std::env::var("WEBAUTHN_RP_NAME") {
        auth_config.rp_name = rp_name;
    }

    // Set rp_id: use explicit env var, or auto-derive from rp_origin's host.
    if let Some(rp_id) = rp_id_explicit {
        auth_config.rp_id = rp_id;
    } else if let Some(host) = server::config::derive_rp_id_from_origin(&auth_config.rp_origin) {
        auth_config.rp_id = host;
    }

    // Validate WebAuthn config at startup to fail fast on misconfiguration
    if !server::config::validate_rp_id(&auth_config.rp_id, &auth_config.rp_origin) {
        tracing::error!(
            rp_id = %auth_config.rp_id,
            rp_origin = %auth_config.rp_origin,
            "WEBAUTHN_RP_ID must be a registrable domain suffix of WEBAUTHN_RP_ORIGIN host — passkey auth will fail"
        );
    }

    tracing::info!(rp_id = %auth_config.rp_id, rp_origin = %auth_config.rp_origin, "WebAuthn configured");

    // Captured here, from the resolved config, because `with_config` below moves
    // it into AuthService and AuthService keeps it private. /health/deep reports
    // these so the deploy check can stop scraping the log line just above - which
    // writes `rp_id` and `=` in separate ANSI escape sequences, so a literal
    // `rp_id=` matches nothing and the first version of that check failed against
    // a perfectly healthy server.
    let webauthn_health = api_types::WebAuthnHealth {
        rp_id: auth_config.rp_id.clone(),
        rp_origin: auth_config.rp_origin.clone(),
    };

    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(&data_service),
        auth_config,
    ));

    // Configure CAPTCHA provider (optional)
    let captcha_provider = match server::config::parse_captcha_env()? {
        Some((_provider, secret, site_key)) => {
            tracing::info!(provider = "turnstile", "CAPTCHA enabled");
            Some(Arc::new(CloudflareTurnstile::new(secret, site_key))
                as Arc<dyn auth::captcha::CaptchaProvider>)
        }
        None => {
            tracing::info!("CAPTCHA disabled (no CAPTCHA_PROVIDER set)");
            None
        }
    };

    // Connect to Redis (REQUIRED for event processing)
    // One explicit expose for the three consumers below (bridge, webhooks,
    // idempotency); the URL stays a secret everywhere else.
    let redis_url = config
        .redis_url
        .as_ref()
        .map(ExposeSecret::expose_secret)
        .ok_or_else(|| anyhow::anyhow!("REDIS_URL is required for event processing"))?;

    tracing::info!("Connecting to Redis...");
    let events_channel =
        std::env::var("REDIS_EVENTS_CHANNEL").unwrap_or_else(|_| EVENTS_CHANNEL.to_string());
    let commands_channel =
        std::env::var("REDIS_COMMANDS_CHANNEL").unwrap_or_else(|_| COMMANDS_CHANNEL.to_string());
    tracing::debug!(
        events_channel,
        commands_channel,
        "Redis channels configured"
    );
    let bridge = Arc::new(RedisBridge::new(redis_url, &events_channel, &commands_channel).await?);
    let live_watches = Arc::new(data_service::RedisDataService::new(redis_url).await?);
    tracing::info!("Redis connected");

    // Create EVM monitor using shared bridge (concrete type for generics)
    let evm_monitor = Arc::new(RedisEVMMonitor::new(Arc::clone(&bridge), live_watches));

    // Create WebSocket broadcast channel (shared by services and HTTP handler)
    let ws_broadcast = Arc::new(server::api::ws::WsBroadcast::new(256));

    // Start background services
    // Bring up the plugin host and load whatever is installed.
    //
    // In safe mode there is no host at all - not an empty one. A boot that
    // builds no wasmtime engine cannot run plugin code by any path, including
    // one added later by someone who did not know to check the flag.
    let plugin_host = if config.safe_mode {
        None
    } else {
        Some(Arc::new(PluginHost::new(
            host_version(),
            DEFAULT_MAX_FAILURES,
            DEFAULT_CALL_DEADLINE,
        )))
    };
    let plugin_artifacts = PluginArtifacts::new(&config.plugin_dir);
    // A failure to *read* the install list is different from a plugin failing
    // to load: the database is not answering, which the rest of the boot is
    // about to discover anyway. Log and continue with no plugins rather than
    // refuse to start - a server that will not come up is the one state an
    // admin cannot fix a plugin problem from.
    // One pool per plugin, all sharing one budget. Built before the loader so
    // a plugin with a credential gets database access as it registers, rather
    // than on a later pass that would leave the first call after boot without
    // it.
    let plugin_pools = Arc::new(PluginPools::new(
        config.database_url.expose_secret().to_string(),
        DEFAULT_MAX_IN_FLIGHT,
    ));

    // The operator store: the stored setting wins, the environment is the
    // fallback.
    //
    // That precedence and not the reverse. An admin who sets this in the UI
    // must see it take effect - if an environment variable silently overrode
    // it, the settings page would show one store while the server issued
    // invoices on another, and nothing would say so. The environment stays
    // supported because instances configured before this setting existed are
    // still configured that way, and because a fresh database has no
    // settings row to read.
    let operator_store_id =
        match auth::ServerSettingsRepository::get_server_settings(&*data_service).await {
            Ok(settings) => settings.and_then(|s| s.operator_store_id).or_else(|| {
                if config.operator_store_id.is_some() {
                    tracing::info!(
                        "operator store taken from ETHPAY_OPERATOR_STORE_ID; setting it in \
                     the admin settings takes precedence from then on"
                    );
                }
                config.operator_store_id
            }),
            // Not fatal. Falling back to the environment is the behaviour this
            // server had before the setting existed, and refusing to start over
            // an unreadable settings row would take the whole instance down for a
            // feature most instances do not use.
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "could not read server settings; falling back to ETHPAY_OPERATOR_STORE_ID"
                );
                config.operator_store_id
            }
        };

    // Published further down, once there is an `AppState` to build an issuer
    // around. Handed to the loader now because this is where a plugin is
    // given its host calls, and a plugin that got none here would have no
    // way to be granted them later.
    let plugin_capabilities = server::services::plugins::DeferredCapabilities::default();
    let plugin_issuer = plugin_capabilities.issuer.clone();

    let loaded = match load_installed_plugins(
        &*data_service,
        plugin_host.as_deref(),
        &plugin_artifacts,
        Some(&plugin_pools),
        &plugin_capabilities,
    )
    .await
    {
        Ok(report) => {
            report_boot(&report);
            report.loaded
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "could not read the installed-plugin list; starting with no plugins loaded"
            );
            Vec::new()
        }
    };

    // Turn the loaded plugins into the capability implementations the server
    // calls - without this, a plugin registers but nothing dispatches to it.
    let (plugin_filters, plugin_payment_observers, plugin_account_closed_observers) = plugin_host
        .as_ref()
        .map_or((Vec::new(), Vec::new(), Vec::new()), |host| {
            (
                invoice_creation_filters(host, &loaded),
                payment_observers(host, &loaded),
                account_closed_observers(host, &loaded),
            )
        });

    // Capability 4 needs both a store to watch and something to tell. Either
    // one missing means no dispatch at all: an instance with an operator
    // store but no plugin has nobody to notify, and observers without a
    // configured store must never be handed a guess at which store is ours.
    let own_store_payments =
        own_store_payment_reporting(operator_store_id, plugin_payment_observers);
    match own_store_payments.as_ref() {
        Some((store_id, observers)) => tracing::info!(
            %store_id,
            observers = observers.len() as u64,
            "own-store payments will be reported to plugins"
        ),
        None => tracing::info!(
            "no own-store payment reporting: ETHPAY_OPERATOR_STORE_ID unset or no plugin loaded"
        ),
    }

    // 1. Webhook delivery service - sends webhook notifications
    //    Created first because cleanup service needs it for expiration webhooks
    let webhook_config = WebhookConfig::from_env();
    tracing::debug!(?webhook_config, "Webhook config loaded");
    let webhook_service = Arc::new(WebhookService::new(
        Arc::clone(&data_service),
        redis_url,
        webhook_config,
    )?);
    let mut webhook_handle = tokio::spawn(Arc::clone(&webhook_service).run());
    tracing::info!("Webhook delivery service started");

    // Drains the transactional outbox `payment_handler` writes a webhook
    // notification obligation into alongside the payment row it is about -
    // see `data_service::webhook_outbox` for why that write is atomic and
    // this one is not. Shares `webhook_service` as its delivery sink, so a
    // drained obligation goes through the exact same queue, retry, and
    // `webhook_deliveries` bookkeeping as one queued synchronously.
    let outbox_drain_service = server::WebhookOutboxDrainService::new(
        Arc::clone(&data_service),
        Arc::clone(&webhook_service) as Arc<dyn server::services::WebhookSink>,
        server::WebhookOutboxDrainConfig::from_env(),
    );
    let mut outbox_drain_handle = tokio::spawn(outbox_drain_service.run());
    tracing::info!("Webhook outbox drain service started");

    // Cloned here rather than where they're used below (the shutdown-signal
    // race near the bottom of `main`) because `webhook_service` is moved
    // into `state.webhook_sink` in the meantime and `bridge` would otherwise
    // need a clone at that call site anyway.
    let bridge_for_shutdown = Arc::clone(&bridge);
    let webhook_service_for_shutdown = Arc::clone(&webhook_service);

    // 2. Invoice cleanup service - expires invoices and unwatches addresses
    //    Also queues webhook notifications when invoices expire
    let cleanup_config = CleanupConfig::from_env();
    tracing::debug!(?cleanup_config, "Cleanup config loaded");
    let cleanup_service = Arc::new(InvoiceCleanupService::new(
        Arc::clone(&data_service),
        Arc::clone(&evm_monitor),
        cleanup_config,
        Some(Arc::clone(&webhook_service)),
        Some(Arc::clone(&ws_broadcast)),
    ));
    tokio::spawn(Arc::clone(&cleanup_service).run());
    tracing::info!("Invoice cleanup service started");

    // 3. Event consumer - processes PaymentDetected/Confirmed events
    //    Also triggers expiration checks on block events and queues webhooks
    let bridge_dyn: Arc<dyn evm::monitor::bridge::EventBridge> = bridge.clone();
    let email_sender = server::services::create_email_sender();
    let event_consumer = EventConsumer::new(
        bridge_dyn,
        Arc::clone(&data_service),
        Some(cleanup_service),
        Some(Arc::clone(&webhook_service) as Arc<dyn server::services::WebhookSink>),
        Some(Arc::clone(&ws_broadcast)),
        Arc::clone(&email_sender),
    );
    // Cloned rather than moved: the same observers are also handed to the
    // reconciliation loop below, which is the pull path under this push one.
    let event_consumer = match own_store_payments.clone() {
        Some((store_id, observers)) => event_consumer.with_own_store_payments(store_id, observers),
        None => event_consumer,
    };
    // An outbox lineage break can hide lost payments; the consumer refuses to
    // resume across one until an operator has audited the gap and opts in.
    // An unrecognised value is refused rather than read as "no": an operator
    // who typed `TRUE` or `yes` meant to opt in and should be told it did not
    // take, not left to work that out from the consumer halting.
    let accept_lineage_break = match std::env::var("EVENT_ACCEPT_LINEAGE_BREAK") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "" | "false" | "0" => false,
            other => {
                anyhow::bail!("EVENT_ACCEPT_LINEAGE_BREAK must be true or false, got {other:?}")
            }
        },
        Err(_) => false,
    };
    let event_consumer = event_consumer.with_accepted_lineage_break(accept_lineage_break);
    // A poison event halts the consumer on every restart; the operator names
    // it (from the halt log) here once the deposit is handled by hand.
    let skipped_events = match std::env::var("EVENT_SKIP_EVENTS") {
        Ok(raw) => server::services::event_consumer::parse_skip_events(&raw).map_err(|e| {
            anyhow::anyhow!("EVENT_SKIP_EVENTS must be chain_id:epoch:seq[,...]: {e}")
        })?,
        Err(_) => Vec::new(),
    };
    let event_consumer = event_consumer.with_skipped_events(skipped_events);
    let mut event_consumer_handle = tokio::spawn(event_consumer.run());
    tracing::info!("Event consumer started");

    // 4. Watch retry service - retries failed WatchAddress commands
    let retry_config = WatchRetryConfig::from_env();
    tracing::debug!(?retry_config, "Watch retry config loaded");
    if retry_config.enabled {
        let retry_service = WatchRetryService::new(
            Arc::clone(&data_service),
            Arc::clone(&evm_monitor),
            retry_config,
        );
        tokio::spawn(retry_service.run());
        tracing::info!("Watch retry service started");
    } else {
        tracing::info!("Watch retry service disabled");
    }

    // 5. Chain health metrics service - polls evmmonitor's published health
    //    data on a timer and exports it as Prometheus gauges, so the gauges
    //    are a fact about the chain rather than a side effect of someone
    //    calling /health/chains.
    let chain_health_metrics_config = ChainHealthMetricsConfig::from_env();
    tracing::debug!(
        ?chain_health_metrics_config,
        "Chain health metrics config loaded"
    );
    let chain_health_metrics_service =
        ChainHealthMetricsService::new(Arc::clone(&evm_monitor), chain_health_metrics_config);
    tokio::spawn(chain_health_metrics_service.run());
    tracing::info!("Chain health metrics service started");

    // Create rate provider
    let rate_config = RateProviderConfig::from_env();
    tracing::info!(provider = rate_config.provider, "Rate provider configured");
    let rate_provider = rate_config.create_provider();

    // Create application state
    let mut state = AppState::new(
        Arc::clone(&data_service),
        auth_service,
        Some(evm_monitor),
        rate_provider,
        email_sender,
    );
    state.ws_broadcast = Some(ws_broadcast);
    state.webhook_sink = Some(webhook_service);
    state.captcha_provider = captcha_provider;
    state.webauthn = Some(webauthn_health);
    state.safe_mode = config.safe_mode;

    state.plugin_host = plugin_host.clone();
    state.plugin_dir = config.plugin_dir.clone();
    // So install and uninstall reach the same pools the boot loader registered.
    state.plugin_pools = Some(Arc::clone(&plugin_pools));
    // The one wired filter call site. Empty until a filter plugin is
    // installed, which is every deployment today; before this line it was
    // empty even then.
    state.invoice_creation_filters = plugin_filters;
    // Capability 8, empty until a plugin is installed, same as the filter list above.
    state.account_closed_observers = plugin_account_closed_observers;
    // Never filtered: see `AppState::operator_store_id`.
    state.operator_store_id = operator_store_id;
    // Checked against every nomination of a new operator store: see
    // `AppState::operator_account_id`.
    state.operator_account_id = config.operator_account_id;

    // Capability 3, published. An instance with no configured operator store
    // publishes nothing, and its plugins are told invoicing is unavailable -
    // which is the truth: there is no store this host would issue on, and
    // guessing at one is how a plugin ends up invoicing a merchant's
    // customers.
    match operator_store_id {
        Some(store_id) => {
            let api = Arc::new(server::services::plugins::PluginHostApi::new(
                state.clone(),
                store_id,
            ));
            let issuer: Arc<dyn server::services::plugins::HostInvoiceIssuer> = api.clone();
            if plugin_issuer.publish(issuer) {
                tracing::info!(%store_id, "plugins may issue invoices on this instance's own store");
            }

            // The pull half of own-store payment reporting. Push is the fast
            // path and never the source of truth: a dispatch is lost whenever
            // the plugin could not take it - disabled after repeated failure,
            // trapped, past its deadline, or not loaded because this process
            // was restarting when the payment confirmed. Every one of those
            // is a merchant who paid and was not credited, and none of them
            // is visible, because the payment itself succeeded.
            if let Some((_, observers)) = own_store_payments.as_ref() {
                let reader: Arc<dyn server::services::plugins::OwnStorePaymentReader> = api;
                tokio::spawn(server::services::plugins::reconcile::run(
                    reader,
                    observers.clone(),
                    server::services::plugins::reconcile::DEFAULT_INTERVAL,
                ));
                tracing::info!(
                    interval_secs =
                        server::services::plugins::reconcile::DEFAULT_INTERVAL.as_secs(),
                    "reconciling own-store payments on a loop; a lost dispatch is caught here"
                );
            }
        }
        None => tracing::info!(
            "plugins cannot issue invoices: ETHPAY_OPERATOR_STORE_ID is unset, so this \
             instance has no store of its own to issue on"
        ),
    }

    // Capability 6, published unconditionally, batched form included. Unlike
    // capability 3 it needs no own store: an instance that sells nothing
    // still has merchants with volume, and a plugin asking what one settled
    // deserves the real answer rather than silence that reads as zero. One
    // reader answers both forms, published under each capability's own cell.
    let volume_reader = Arc::new(server::services::plugins::PluginMerchantVolume::new(
        state.clone(),
    ));
    if plugin_capabilities.volume.publish(volume_reader.clone()) {
        tracing::info!("plugins may read what an account settled over a window");
    }
    if plugin_capabilities.bulk_volume.publish(volume_reader) {
        tracing::info!("plugins may read what many accounts settled in one call");
    }

    // Capability 7: the standing the host stored for an account, read-only.
    // Needs only the data layer, so it is published unconditionally; a plugin
    // can read a standing and has no import by which to write one.
    if plugin_capabilities
        .standing
        .publish(state.data_service.clone())
    {
        tracing::info!("plugins may read an account's stored standing");
    }

    // Capability 5. `PageHost` is built empty by `AppState::new` and has
    // never had a production renderer registered in it, so every plugin page
    // request 404'd - correct for a host with nothing to draw, and
    // indistinguishable from a feature that was never wired up.
    if let Some(host) = plugin_host.as_ref() {
        let mut pages = server::services::plugins::PageHost::new();
        for id in &loaded {
            pages.register(
                id.clone(),
                Arc::new(server::services::plugins::WasmPageRenderer::new(
                    Arc::clone(host),
                    id.clone(),
                )),
            );
        }
        if !loaded.is_empty() {
            tracing::info!(
                plugins = loaded.len() as u64,
                "plugin pages are served from /plugins/{{id}}/pages/{{path}}"
            );
        }
        state.plugin_pages = Arc::new(pages);
    }

    // Create rate limiters
    let rate_limit_config = RateLimitConfig::from_env();
    tracing::info!(?rate_limit_config, "Rate limiting configured");
    let rate_limiters = Arc::new(RateLimitState::from_config(&rate_limit_config));

    // Create idempotency state (shares the existing Redis connection)
    let idempotency = match api::idempotency::IdempotencyState::from_env(redis_url).await {
        Ok(idem) => {
            tracing::info!(ttl_secs = idem.ttl_secs, "Idempotency middleware enabled");
            Some(Arc::new(idem))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create idempotency state, disabled");
            None
        }
    };

    // Create per-API-key rate limiter
    let api_key_rate_limiter =
        Arc::new(ApiKeyRateLimitState::from_env(data_service.pool().clone()));
    tracing::info!(
        default_rpm = api_key_rate_limiter.default_rpm,
        "Per-API-key rate limiting configured"
    );

    // Build router with middleware. `with_sentry_performance_tracing` adds
    // request tracing, CORS, and the Sentry transaction layers together, in
    // the one order this binary and its test both use.
    let app = api::with_sentry_performance_tracing(api::router(
        state,
        config.enable_swagger,
        Some(rate_limiters),
        idempotency,
        Some(api_key_rate_limiter),
    ));

    // Start server
    let bind_addr = config.bind_address();
    tracing::info!("Server listening on http://{}", bind_addr);

    if config.enable_swagger {
        tracing::info!("Swagger UI available at http://{}/swagger-ui", bind_addr);
    }

    let listener = TcpListener::bind(&bind_addr).await?;

    // Races the server against the stop signal rather than wrapping it in
    // axum's `with_graceful_shutdown`: that drains in-flight HTTP requests
    // for as long as they take, and `shutting_down` would have to flip before
    // the drain starts to cover the redis/webhook paths below — leaving it
    // true for that whole open-ended window, during which an unrelated real
    // fault would also log as shutdown noise. Racing keeps the flag's "we
    // asked to stop" window bounded to the same order as evmmonitor's below.
    tokio::select! {
        result = axum::serve(listener, app) => {
            result?;
        }
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            bridge_for_shutdown.begin_shutdown();
        }
    }

    // Give the events subscription the same chance evmmonitor's command
    // subscription gets: a moment to notice its own connection ending and
    // log itself as a shutdown (see redis.rs's `subscribe` tail) before the
    // process exits out from under it. Without this wait, `main` could
    // return before the compose network teardown actually breaks the redis
    // connection, so the task is simply dropped mid-poll and its tail — the
    // info/error decision this whole change is about — never runs at all.
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        &mut event_consumer_handle,
    )
    .await
    {
        Err(_) => {
            tracing::debug!(
                "event consumer task did not exit within the shutdown grace period, aborting"
            );
            event_consumer_handle.abort();
        }
        Ok(Err(join_error)) => {
            tracing::error!(error = %join_error, "event consumer task ended unexpectedly during shutdown");
        }
        Ok(Ok(())) => {}
    }

    // The webhook worker's loop never returns on its own (see `run()` in
    // service.rs) — it keeps draining the queue for as long as it's alive,
    // which is the point, so this wait always ends in the timeout branch,
    // with the task still running completely normally for the whole second.
    // What it buys is the same thing as the event consumer's wait: a moment
    // for `process_next_job`'s in-flight call to finish and log itself as
    // shutdown noise before the task is torn down, rather than being dropped
    // mid-poll by process exit with no handle ever joined on it at all.
    //
    // `begin_shutdown()` is deliberately *not* set before this wait. Nothing
    // about this task's own teardown has started yet during it — the loop is
    // still polling Redis and delivering webhooks exactly as it would outside
    // a shutdown — so a real fault landing in that ~1s would otherwise log as
    // shutdown noise for no reason. It's set right before `abort()` instead,
    // which is the point past which a Redis error actually is teardown
    // fallout rather than a coincidence.
    match tokio::time::timeout(std::time::Duration::from_secs(1), &mut webhook_handle).await {
        Err(_) => {
            tracing::debug!(
                "webhook worker task did not exit within the shutdown grace period, aborting"
            );
            webhook_service_for_shutdown.begin_shutdown();
            webhook_handle.abort();
        }
        Ok(Err(join_error)) => {
            tracing::error!(error = %join_error, "webhook worker task ended unexpectedly during shutdown");
        }
        Ok(Ok(())) => {}
    }

    // Same reasoning as the event consumer's wait above: the drain's loop
    // never returns on its own, so if it ever did - a panic, a poisoned lock
    // - that is exactly the kind of silent stop this service exists to
    // prevent one layer down (a written obligation with nothing left polling
    // for it), so it must be joined and logged rather than dropped bare.
    match tokio::time::timeout(std::time::Duration::from_secs(1), &mut outbox_drain_handle).await {
        Err(_) => {
            tracing::debug!(
                "webhook outbox drain task did not exit within the shutdown grace period, aborting"
            );
            outbox_drain_handle.abort();
        }
        Ok(Err(join_error)) => {
            tracing::error!(error = %join_error, "webhook outbox drain task ended unexpectedly during shutdown");
        }
        Ok(Ok(())) => {}
    }

    Ok(())
}

/// Resolves once the process receives a stop signal (Ctrl+C or SIGTERM).
///
/// Raced against `axum::serve(...)` — this process has no other way to learn
/// its container was asked to stop, and installing these handlers replaces
/// the OS's default terminate-on-SIGTERM action, so something has to make
/// the process actually exit afterward.
async fn shutdown_signal() {
    // Installing these handlers only fails if the OS refuses to let the
    // process register a signal handler at all, which would mean nothing
    // else in this process can be trusted to work either.
    #[allow(
        clippy::expect_used,
        reason = "handler registration failure is unrecoverable at startup"
    )]
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    #[allow(
        clippy::expect_used,
        reason = "handler registration failure is unrecoverable at startup"
    )]
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
