//! PostgreSQL implementation of the repository traits.

use chrono::{DateTime, Utc};
use metrics::histogram;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgPool, Postgres};
use uuid::Uuid;

mod account_standing;
mod auth;
mod chain_cursor;
mod conversions;
mod email_change;
mod expected_watch;
mod installed_plugins;
mod invoice;
mod invoice_creation;
mod merchant_directory;
mod payment;
mod payment_option;
mod payout;
mod refund;
mod settlement_tolerance;
pub(crate) mod store_creation;
mod store_invites;
mod store_payment_method;
mod store_settings;
mod store_token_policy;
mod store_webhook;
mod token;
pub(crate) mod wallet;
mod wallet_rotation;
mod watched_address;
mod watched_address_deletion;
mod webhook_delivery;
mod webhook_outbox;

pub use auth::{ApiKeyRateLimitInfo, PostgresApiKeyRepository, WalletReauthChallenge};
pub use expected_watch::ExpectedWatch;
pub use wallet_rotation::WalletRotation;
pub use watched_address::PendingWatch;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod tests;

// =============================================================================
// List search
// =============================================================================

/// Escape the characters `LIKE` treats as wildcards, and lower-case the term.
///
/// Without this a merchant typing `%` matches every row, and `_` matches any
/// character - a search box is not a pattern language. Backslash is `LIKE`'s
/// default escape character, so escaping it first keeps a literal backslash
/// literal without needing an `ESCAPE` clause.
///
/// Lower-casing here rather than reaching for `ILIKE` is what keeps the
/// predicate index-able: `ILIKE` can use no btree index at all, while
/// `LOWER(col) LIKE 'x%'` can be served by an expression index - the shape the
/// existing `idx_watched_lower_addr_chain` and `idx_tokens_symbol` already use.
/// Terms are ASCII hex, currency codes and asset symbols, where Rust's
/// `to_lowercase` and Postgres' `LOWER` agree.
fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    for ch in term.to_lowercase().chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// `term%` - an anchored prefix, for columns holding an identifier the user
/// pastes whole (tx hashes, invoice ids) and where an index can serve the scan.
pub(super) fn search_prefix_pattern(term: &str) -> String {
    format!("{}%", escape_like(term))
}

/// `%term%` - a substring, for short columns where a partial match is what the
/// user means (asset symbols, currencies, amounts) and where anchoring the
/// pattern would buy no index anyway.
pub(super) fn search_contains_pattern(term: &str) -> String {
    format!("%{}%", escape_like(term))
}

/// PostgreSQL data service implementation.
#[derive(Clone)]
pub struct PgDataService {
    pool: PgPool,
}

impl PgDataService {
    /// Create a new PgDataService with the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connect to PostgreSQL with the given database URL.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPool::connect(database_url).await?;
        Ok(Self::new(pool))
    }

    /// Get a reference to the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Acquire a connection from the pool, recording wait duration as a metric.
    pub async fn acquire(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
        let start = std::time::Instant::now();
        let conn = self.pool.acquire().await;
        histogram!("ethpayserver_db_pool_wait_duration_seconds")
            .record(start.elapsed().as_secs_f64());
        conn
    }

    /// Check database connectivity.
    pub async fn health_check(&self) -> Result<(), sqlx::Error> {
        let mut conn = self.acquire().await?;
        sqlx::query("SELECT 1").execute(&mut *conn).await?;
        Ok(())
    }

    /// Count total registered users.
    pub async fn count_users(&self) -> Result<i64, sqlx::Error> {
        let mut conn = self.acquire().await?;
        let result: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&mut *conn)
            .await?;
        Ok(result.0)
    }

    /// Count total stores (non-archived).
    pub async fn count_stores(&self) -> Result<i64, sqlx::Error> {
        let mut conn = self.acquire().await?;
        let result: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stores WHERE archived = false")
            .fetch_one(&mut *conn)
            .await?;
        Ok(result.0)
    }

    /// Look up an active API key's ID and rate limit by its hash.
    pub async fn get_api_key_rate_limit_by_hash(
        &self,
        key_hash: &str,
    ) -> ::auth::error::Result<Option<ApiKeyRateLimitInfo>> {
        PostgresApiKeyRepository::new(self.pool.clone())
            .get_rate_limit_by_hash(key_hash)
            .await
    }

    /// Update the per-key rate limit for an API key.
    pub async fn update_api_key_rate_limit(
        &self,
        id: ::auth::ApiKeyId,
        rpm: Option<i32>,
    ) -> ::auth::error::Result<()> {
        PostgresApiKeyRepository::new(self.pool.clone())
            .update_rate_limit(id, rpm)
            .await
    }

    /// List all API keys for a user, including rate_limit_rpm.
    pub async fn list_user_api_keys_with_rate_limit(
        &self,
        user_id: ::auth::UserId,
    ) -> ::auth::error::Result<Vec<(::auth::ApiKey, Option<i32>)>> {
        PostgresApiKeyRepository::new(self.pool.clone())
            .list_user_api_keys_with_rate_limit(user_id)
            .await
    }

    /// Create a new API key with an initial permission scope. See
    /// `PostgresApiKeyRepository::create_api_key_with_permissions`.
    pub async fn create_api_key_with_permissions(
        &self,
        key: &::auth::ApiKey,
        permissions: Option<&[String]>,
    ) -> ::auth::error::Result<()> {
        PostgresApiKeyRepository::new(self.pool.clone())
            .create_api_key_with_permissions(key, permissions)
            .await
    }
}

// === API Key Rotation / Deprecation ===

/// Auth info returned when validating an API key.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKeyAuthInfo {
    pub id: Uuid,
    pub user_id: Uuid,
    pub is_active: bool,
    pub deprecated_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Per-key rate limit in requests per minute. Null = server default.
    pub rate_limit_rpm: Option<i32>,
    /// Explicitly granted, never inherited from role or owner. See the
    /// `is_operator` column comment on `api_keys`.
    pub is_operator: bool,
    /// Permission policy strings this key is scoped to. `None` means
    /// "inherit the owner's role in full" - see the migration that added
    /// this column.
    pub permissions: Option<Vec<String>>,
}

/// Full API key info for listing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKeyFullInfo {
    pub id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub deprecated_at: Option<DateTime<Utc>>,
    /// Per-key rate limit in requests per minute. Null = server default.
    pub rate_limit_rpm: Option<i32>,
    /// Permission policy strings this key is scoped to. `None` means
    /// "inherit the owner's role in full".
    pub permissions: Option<Vec<String>>,
}

impl PgDataService {
    /// Look up an API key by its SHA-256 hash for authentication.
    pub async fn get_api_key_auth_info(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyAuthInfo>, sqlx::Error> {
        sqlx::query_as::<_, ApiKeyAuthInfo>(
            "SELECT id, user_id, is_active, deprecated_at, expires_at, rate_limit_rpm, \
                    is_operator, permissions \
             FROM api_keys WHERE key_hash = $1",
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
    }

    /// List all API keys for a user (with deprecation info and per-key rate limit).
    pub async fn list_user_api_keys_full(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<ApiKeyFullInfo>, sqlx::Error> {
        sqlx::query_as::<_, ApiKeyFullInfo>(
            "SELECT id, name, key_prefix, is_active, created_at, last_used_at, \
                    expires_at, deprecated_at, rate_limit_rpm, permissions \
             FROM api_keys WHERE user_id = $1 ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Mark an API key as deprecated (starts the grace window).
    pub async fn set_api_key_deprecated(
        &self,
        id: Uuid,
        deprecated_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE api_keys SET deprecated_at = $1 WHERE id = $2")
            .bind(deprecated_at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Atomically create a replacement API key and mark the old key as
    /// deprecated. Wraps the insert + update in a single transaction so we
    /// cannot end up with two active keys (new created, old still active)
    /// if the second statement fails.
    ///
    /// `permissions` carries over the old key's permission scope (or lack
    /// of one) to the replacement - rotation is meant to swap the secret,
    /// not quietly widen what it can do.
    ///
    /// Returns `Ok(())` on success. On failure the transaction rolls back
    /// and the database is left unchanged.
    pub async fn rotate_api_key_atomic(
        &self,
        new_key: &::auth::ApiKey,
        permissions: Option<&[String]>,
        old_id: Uuid,
        deprecated_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Insert the replacement key.
        sqlx::query(
            "INSERT INTO api_keys \
               (id, user_id, name, key_hash, key_prefix, is_active, \
                created_at, last_used_at, expires_at, permissions) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(new_key.id.0)
        .bind(new_key.user_id.0)
        .bind(&new_key.name)
        .bind(&new_key.key_hash)
        .bind(&new_key.key_prefix)
        .bind(new_key.is_active)
        .bind(new_key.created_at)
        .bind(new_key.last_used_at)
        .bind(new_key.expires_at)
        .bind(permissions)
        .execute(&mut *tx)
        .await?;

        // Mark the old key deprecated — only succeeds if old_id exists and
        // is not already deprecated, so concurrent rotates don't double-
        // deprecate (whichever commits first wins; the other sees
        // `rows_affected == 0` and aborts).
        let result = sqlx::query(
            "UPDATE api_keys SET deprecated_at = $1 WHERE id = $2 AND deprecated_at IS NULL",
        )
        .bind(deprecated_at)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            // Another rotate deprecated this key between our pre-check and
            // now, or the row vanished. Roll back the inserted new key.
            tx.rollback().await?;
            return Err(sqlx::Error::RowNotFound);
        }

        tx.commit().await?;
        Ok(())
    }
}

impl PgDataService {
    /// Look up an API key's auth info by its primary key ID (for rotation checks).
    pub async fn get_api_key_auth_info_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<ApiKeyAuthInfo>, sqlx::Error> {
        sqlx::query_as::<_, ApiKeyAuthInfo>(
            "SELECT id, user_id, is_active, deprecated_at, expires_at, rate_limit_rpm, \
                    is_operator, permissions \
             FROM api_keys WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }
}
