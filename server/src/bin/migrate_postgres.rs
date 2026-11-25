//! Database migration binary.
//!
//! Run migrations with:
//! ```bash
//! DATABASE_URL="postgres://..." cargo run --bin migrate
//! ```

use anyhow::Result;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use data_service::PgDataService;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    let _ = dotenvy::dotenv();

    // Initialize tracing
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Get database URL
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is required"))?;

    // xpub encryption at rest (#581). Needed before the backfill below, which
    // runs in this same binary right after the schema migration that adds
    // room for it - plain SQL has no key to encrypt with, so this binary is
    // the only place that backfill can happen.
    data_service::xpub_encryption::init(data_service::xpub_encryption::provider_from_env().await?);

    // Connect to database
    tracing::info!("Connecting to database...");
    let data_service = PgDataService::connect(&database_url).await?;
    tracing::info!("Database connected");

    // Run migrations
    tracing::info!("Running database migrations...");
    sqlx::migrate!("../data-service/migrations/postgres")
        .run(data_service.pool())
        .await?;
    tracing::info!("Database migrations completed");

    backfill_and_finalize_wallet_encryption(data_service.pool()).await?;

    Ok(())
}

/// Finishes what `20261125000000_encrypt_wallet_xpub.sql` could not: that
/// migration can only add nullable columns, because plain SQL has no key to
/// encrypt the existing plaintext `xpub` column with. This runs right after
/// it, in the same deploy, before `server`/`evmmonitor` start - encrypts
/// every row the migration left as ciphertext-NULL, then tightens the new
/// columns to NOT NULL and drops the plaintext column.
///
/// Idempotent: skips entirely once the plaintext `xpub` column is gone, so a
/// second run (the next deploy, or a retry after a crash) is a cheap no-op
/// rather than a second attempt to re-encrypt already-migrated rows or to
/// drop a column that is already gone.
#[allow(clippy::cognitive_complexity)] // linear one-time backfill script, not branchy
async fn backfill_and_finalize_wallet_encryption(pool: &sqlx::PgPool) -> Result<()> {
    if !legacy_plaintext_column_exists(pool).await? {
        tracing::info!("wallets.xpub already encrypted and finalized; nothing to backfill");
        return Ok(());
    }

    tracing::info!("Backfilling xpub encryption for existing wallets (#581)...");
    let mut tx = pool.begin().await?;
    let count = encrypt_legacy_wallet_rows(&mut tx).await?;
    tracing::info!("Encrypted {count} existing wallet xpub(s). Finalizing schema...");
    finalize_wallet_encryption_schema(&mut tx).await?;
    tx.commit().await?;
    tracing::info!("xpub encryption backfill complete; plaintext column dropped");
    Ok(())
}

/// Whether the plaintext `xpub` column this migration is replacing is still
/// there. False means a previous deploy already backfilled and dropped it.
async fn legacy_plaintext_column_exists(pool: &sqlx::PgPool) -> Result<bool> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM information_schema.columns
            WHERE table_name = 'wallets' AND column_name = 'xpub'
        )",
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Encrypts every row the migration left as ciphertext-NULL, in place.
/// Returns how many rows it touched, purely for the log line above.
async fn encrypt_legacy_wallet_rows(tx: &mut sqlx::PgConnection) -> Result<usize> {
    // FOR UPDATE: held for the rest of the caller's transaction, which also
    // does the DDL in `finalize_wallet_encryption_schema`. Nothing else
    // writes `wallets` concurrently during a deploy (server/evmmonitor have
    // not started yet), but this is cheap insurance against a stray manual
    // query doing so.
    let rows: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, xpub FROM wallets WHERE xpub_ciphertext IS NULL FOR UPDATE")
            .fetch_all(&mut *tx)
            .await?;

    let count = rows.len();
    for (id, xpub) in rows {
        let (blob, fingerprint) = data_service::xpub_encryption::encrypt_and_fingerprint(&xpub)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        sqlx::query(
            "UPDATE wallets SET xpub_ciphertext = $1, xpub_iv = $2, xpub_mac = $3, \
             xpub_fingerprint = $4 WHERE id = $5",
        )
        .bind(&blob.ciphertext)
        .bind(&blob.iv)
        .bind(&blob.mac)
        .bind(&fingerprint)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    Ok(count)
}

/// Tightens the new columns to NOT NULL and drops the now-redundant
/// plaintext column. Only reached once every row has a ciphertext, so the
/// NOT NULL constraints hold the moment they are added.
async fn finalize_wallet_encryption_schema(tx: &mut sqlx::PgConnection) -> Result<()> {
    for statement in [
        "ALTER TABLE wallets ALTER COLUMN xpub_ciphertext SET NOT NULL",
        "ALTER TABLE wallets ALTER COLUMN xpub_iv SET NOT NULL",
        "ALTER TABLE wallets ALTER COLUMN xpub_mac SET NOT NULL",
        "ALTER TABLE wallets ALTER COLUMN xpub_fingerprint SET NOT NULL",
        "ALTER TABLE wallets DROP COLUMN xpub",
    ] {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    Ok(())
}
