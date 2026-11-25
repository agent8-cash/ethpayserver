//! ETHPayServer MCP server binary.
//!
//! Exposes invoice and payment operations as MCP tools for AI agents.
//!
//! # Configuration
//!
//! Required environment variables:
//! - `DATABASE_URL` — PostgreSQL connection string
//! - `ETHPAY_API_KEY` — API key for authentication (format: `ak_live_...`)
//!
//! Optional:
//! - `REDIS_URL` — Redis connection for EVM address watching (enables full invoice creation)
//! - `RATE_PROVIDER` — Exchange rate provider: "kraken", "coingecko", "none" (default: "kraken")
//!
//! # Usage
//!
//! ```bash
//! DATABASE_URL="postgres://..." ETHPAY_API_KEY="ak_live_..." ethpay-mcp
//! ```

#[cfg(test)]
mod auth_tests;
mod server;
#[cfg(test)]
mod testkit;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tracing_subscriber::{self, EnvFilter};

use auth::{ApiKeyRepository, StoreRepository, UserId};
use data_service::PgDataService;
use rates::RateProviderConfig;
use rmcp::{ServiceExt, transport::stdio};
use types::StoreId;

use crate::server::EthpayMcpServer;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    // MCP servers MUST log to stderr (stdout is the JSON-RPC transport)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting ethpay-mcp server");

    // xpub encryption at rest. Fail-closed like the real server's own
    // startup check: invoice creation here goes through the same
    // `allocate_derivation` -> decrypt path, so this binary needs the same
    // key provider or every invoice-creation tool call fails at the first
    // wallet it touches.
    data_service::xpub_encryption::init(data_service::xpub_encryption::provider_from_env().await?);

    // Required: database
    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL environment variable required")?;
    let data_service = Arc::new(PgDataService::connect(&database_url).await?);
    tracing::info!("Database connected");

    // Required: API key
    let api_key_raw =
        std::env::var("ETHPAY_API_KEY").context("ETHPAY_API_KEY environment variable required")?;
    let (user_id, store_ids) = validate_api_key(&*data_service, &api_key_raw).await?;
    tracing::info!(
        user_id = %user_id.0,
        stores = store_ids.len(),
        "API key validated"
    );

    // Optional: rate provider (for invoice creation with currency conversion)
    let rate_config = RateProviderConfig::from_env();
    let rate_provider = rate_config.create_provider();

    // Optional: EVM monitor via Redis (for watching addresses on invoice creation)
    let evm_monitor = match std::env::var("REDIS_URL") {
        Ok(redis_url) => match server::create_evm_monitor(&redis_url).await {
            Ok(monitor) => {
                tracing::info!("EVM monitor connected via Redis");
                Some(monitor)
            }
            Err(e) => {
                tracing::warn!(
                    "EVM monitor unavailable: {e} \
                         — invoices created without address watching"
                );
                None
            }
        },
        Err(_) => {
            tracing::info!("REDIS_URL not set — invoices will be created without address watching");
            None
        }
    };

    let server = EthpayMcpServer::new(data_service, user_id, store_ids, rate_provider, evm_monitor);

    let service = server
        .serve(stdio())
        .await
        .context("Failed to start MCP service")?;

    service.waiting().await?;
    Ok(())
}

/// Validate an API key and return the owning user ID and their accessible store IDs.
///
/// Generic over the repositories (rather than naming `PgDataService`) so the
/// auth tests can drive it with a stub instead of a database.
async fn validate_api_key(
    data_service: &(impl ApiKeyRepository + StoreRepository),
    raw_key: &str,
) -> Result<(UserId, Vec<StoreId>)> {
    // Hash the key (same as auth crate: SHA-256 hex)
    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    let key_hash = hex::encode(hasher.finalize());

    // Look up the key
    let api_key = data_service
        .get_api_key_by_hash(&key_hash)
        .await
        .context("Database error looking up API key")?
        .context("Invalid API key")?;

    if !api_key.is_active {
        bail!("API key is deactivated");
    }

    if let Some(expires_at) = api_key.expires_at
        && expires_at < chrono::Utc::now()
    {
        bail!("API key has expired");
    }

    // Update last_used timestamp
    let _ = data_service.update_last_used(api_key.id).await;

    let user_id = api_key.user_id;

    // Get all stores the user has access to
    let stores = data_service
        .get_stores_for_user(user_id)
        .await
        .context("Failed to fetch user stores")?;
    let store_ids: Vec<StoreId> = stores.into_iter().map(|s| s.id).collect();

    Ok((user_id, store_ids))
}
