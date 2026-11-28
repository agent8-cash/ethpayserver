//! Periodic health publishing to Redis.

use std::sync::Arc;

use evm::monitor::{MonitorCoordinator, RpcBlockSource};
use tracing::{debug, error, info, warn};

/// Redis key for publishing health information.
const HEALTH_KEY: &str = "evmmonitor:health";

/// Redis key for publishing the compiled `SENTRY_RELEASE`.
///
/// evmmonitor's only HTTP surface is the loopback Prometheus `/metrics`
/// added for RPC-volume benchmarking, which exposes counters/histograms,
/// not arbitrary values like this - so this reuses the same Redis-relay
/// channel as `HEALTH_KEY` to make its compiled release
/// observable from the API server's `/health/deep`, the same way the
/// server's own `SENTRY_RELEASE` is exposed as a response header - rather
/// than trusting the CI build log for a binary evmmonitor also feeds Sentry
/// from independently.
const SENTRY_RELEASE_KEY: &str = "evmmonitor:sentry_release";

/// Periodically publish health information to Redis.
///
/// Health info is stored as JSON in the HEALTH_KEY with a 60-second TTL.
/// This allows the API server to read health without direct communication.
pub(crate) async fn publish_health_loop(
    coordinator: &Arc<MonitorCoordinator<RpcBlockSource>>,
    redis_url: &str,
    sentry_release: &str,
) {
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to connect to redis for health publishing");
            return;
        }
    };

    // Get connection once and reuse it (ConnectionManager handles reconnects)
    let mut conn = match client.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to get redis connection for health publishing");
            return;
        }
    };

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    info!("health publisher started");

    loop {
        interval.tick().await;

        let health = coordinator.get_all_health().await;

        let health_json = match serde_json::to_string(&health) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "failed to serialize health");
                continue;
            }
        };

        // SET with 60 second expiry
        let published = redis::cmd("SETEX")
            .arg(HEALTH_KEY)
            .arg(60)
            .arg(&health_json)
            .query_async::<()>(&mut conn)
            .await
            .and(
                redis::cmd("SETEX")
                    .arg(SENTRY_RELEASE_KEY)
                    .arg(60)
                    .arg(sentry_release)
                    .query_async::<()>(&mut conn)
                    .await,
            );

        if let Err(e) = published {
            warn!(error = %e, "failed to publish health to redis");
            // Try to reconnect on next iteration
            match client.get_multiplexed_async_connection().await {
                Ok(new_conn) => {
                    conn = new_conn;
                    debug!("reconnected to redis for health publishing");
                }
                Err(e) => {
                    warn!(error = %e, "failed to reconnect to redis");
                }
            }
        }
    }
}
