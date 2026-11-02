//! Watched address repository implementation.

use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

use crate::{
    CleanupAddressInfo, PendingWatchInfo, RepositoryResult, WatchedAddressReader,
    WatchedAddressWriter, sqlx_to_repo_error,
};
use types::{InvoiceId, PaymentOptionId};

use super::PgDataService;
use super::conversions::chain_id_from_row;

#[async_trait]
impl WatchedAddressReader for PgDataService {
    async fn get_invoice_id(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<Option<InvoiceId>> {
        let row = if let Some(token) = token_address {
            sqlx::query(
                r#"
                SELECT po.invoice_id
                FROM watched_addresses wa
                JOIN payment_options po ON wa.payment_option_id = po.id
                WHERE LOWER(wa.address) = LOWER($1) AND wa.chain_id = $2 AND LOWER(wa.token_address) = LOWER($3) AND wa.is_active = TRUE
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .bind(token)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
        } else {
            sqlx::query(
                r#"
                SELECT po.invoice_id
                FROM watched_addresses wa
                JOIN payment_options po ON wa.payment_option_id = po.id
                WHERE LOWER(wa.address) = LOWER($1) AND wa.chain_id = $2 AND wa.token_address IS NULL AND wa.is_active = TRUE
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
        };

        Ok(row.map(|r| InvoiceId::from_string(r.get("invoice_id"))))
    }

    async fn get_payment_option_id(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<Option<PaymentOptionId>> {
        let row = if let Some(token) = token_address {
            sqlx::query(
                r#"
                SELECT payment_option_id
                FROM watched_addresses
                WHERE LOWER(address) = LOWER($1) AND chain_id = $2 AND LOWER(token_address) = LOWER($3) AND is_active = TRUE
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .bind(token)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
        } else {
            sqlx::query(
                r#"
                SELECT payment_option_id
                FROM watched_addresses
                WHERE LOWER(address) = LOWER($1) AND chain_id = $2 AND token_address IS NULL AND is_active = TRUE
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
        };

        Ok(row.map(|r| {
            let id: Uuid = r.get("payment_option_id");
            PaymentOptionId(id)
        }))
    }

    async fn get_pending(&self) -> RepositoryResult<Vec<PendingWatchInfo>> {
        let rows = sqlx::query(
            r#"
            SELECT
                wa.address,
                wa.payment_option_id,
                po.invoice_id,
                wa.chain_id,
                wa.token_address,
                po.amount::text as expected_amount
            FROM watched_addresses wa
            JOIN payment_options po ON wa.payment_option_id = po.id
            WHERE wa.is_active = TRUE
              AND wa.monitor_notified = FALSE
              AND wa.expires_at > NOW()
            ORDER BY wa.created_at ASC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let payment_option_id: Uuid = r.get("payment_option_id");
            let chain_id = chain_id_from_row(r, "chain_id");
            result.push(PendingWatchInfo {
                address: r.get("address"),
                payment_option_id: PaymentOptionId(payment_option_id),
                invoice_id: r.get("invoice_id"),
                chain_id,
                expected_amount: r.get("expected_amount"),
                token_address: r.get("token_address"),
            });
        }
        Ok(result)
    }

    async fn get_expired_for_cleanup(
        &self,
        grace_period_secs: i64,
    ) -> RepositoryResult<Vec<CleanupAddressInfo>> {
        let rows = sqlx::query(
            r#"
            SELECT wa.address, wa.payment_option_id, wa.chain_id, wa.token_address, po.invoice_id
            FROM watched_addresses wa
            JOIN payment_options po ON wa.payment_option_id = po.id
            JOIN invoices i ON po.invoice_id = i.id
            WHERE wa.is_active = TRUE
              AND i.status = 'expired'
              AND i.expires_at < NOW() - make_interval(secs => $1)
            ORDER BY i.expires_at ASC
            LIMIT 100
            "#,
        )
        .bind(grace_period_secs as f64)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let payment_option_id: Uuid = r.get("payment_option_id");
            let chain_id = chain_id_from_row(r, "chain_id");
            result.push(CleanupAddressInfo {
                address: r.get("address"),
                payment_option_id: PaymentOptionId(payment_option_id),
                invoice_id: r.get("invoice_id"),
                chain_id,
                token_address: r.get("token_address"),
            });
        }
        Ok(result)
    }

    async fn get_paid_for_cleanup(&self) -> RepositoryResult<Vec<CleanupAddressInfo>> {
        let rows = sqlx::query(
            r#"
            SELECT wa.address, wa.payment_option_id, wa.chain_id, wa.token_address, po.invoice_id
            FROM watched_addresses wa
            JOIN payment_options po ON wa.payment_option_id = po.id
            JOIN invoices i ON po.invoice_id = i.id
            WHERE wa.is_active = TRUE
              AND i.status = 'paid'
            ORDER BY i.created_at ASC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let payment_option_id: Uuid = r.get("payment_option_id");
            let chain_id = chain_id_from_row(r, "chain_id");
            result.push(CleanupAddressInfo {
                address: r.get("address"),
                payment_option_id: PaymentOptionId(payment_option_id),
                invoice_id: r.get("invoice_id"),
                chain_id,
                token_address: r.get("token_address"),
            });
        }
        Ok(result)
    }

    async fn get_cancelled_for_cleanup(&self) -> RepositoryResult<Vec<CleanupAddressInfo>> {
        let rows = sqlx::query(
            r#"
            SELECT wa.address, wa.payment_option_id, wa.chain_id, wa.token_address, po.invoice_id
            FROM watched_addresses wa
            JOIN payment_options po ON wa.payment_option_id = po.id
            JOIN invoices i ON po.invoice_id = i.id
            WHERE wa.is_active = TRUE
              AND i.status = 'cancelled'
            ORDER BY i.created_at ASC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        let mut result = Vec::with_capacity(rows.len());
        for r in &rows {
            let payment_option_id: Uuid = r.get("payment_option_id");
            let chain_id = chain_id_from_row(r, "chain_id");
            result.push(CleanupAddressInfo {
                address: r.get("address"),
                payment_option_id: PaymentOptionId(payment_option_id),
                invoice_id: r.get("invoice_id"),
                chain_id,
                token_address: r.get("token_address"),
            });
        }
        Ok(result)
    }
}

#[async_trait]
impl WatchedAddressWriter for PgDataService {
    #[allow(clippy::too_many_lines)] // ERC20 vs native-asset branches each need their own SQL + tx
    async fn upsert(
        &self,
        address: &str,
        payment_option_id: &PaymentOptionId,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<()> {
        // Get the invoice's expiration for the watched address
        let invoice_row = sqlx::query(
            r#"
            SELECT i.expires_at
            FROM payment_options po
            JOIN invoices i ON po.invoice_id = i.id
            WHERE po.id = $1
            "#,
        )
        .bind(payment_option_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        let expires_at = invoice_row
            .map(|r| r.get("expires_at"))
            .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(24));

        // Get the invoice_id from the payment option for the watched_addresses table
        let invoice_id_row = sqlx::query(r#"SELECT invoice_id FROM payment_options WHERE id = $1"#)
            .bind(payment_option_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?;

        let invoice_id: String = invoice_id_row
            .map(|r| r.get("invoice_id"))
            .unwrap_or_default();

        // The branching statements live in `invoice_creation`, shared with the
        // transactional path. Only the two lookups above are specific to this
        // caller: inside a transaction the invoice is not committed yet, so
        // there is nothing to look up and the values are passed in instead.
        let mut conn = self.pool.acquire().await.map_err(sqlx_to_repo_error)?;
        super::invoice_creation::upsert_watched_address(
            &mut conn,
            &invoice_id,
            expires_at,
            address,
            payment_option_id,
            chain_id,
            token_address,
        )
        .await?;

        Ok(())
    }

    async fn mark_notified(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<()> {
        if let Some(token) = token_address {
            sqlx::query(
                r#"
                UPDATE watched_addresses
                SET monitor_notified = TRUE
                WHERE LOWER(address) = LOWER($1) AND chain_id = $2 AND LOWER(token_address) = LOWER($3)
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .bind(token)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?;
        } else {
            sqlx::query(
                r#"
                UPDATE watched_addresses
                SET monitor_notified = TRUE
                WHERE LOWER(address) = LOWER($1) AND chain_id = $2 AND token_address IS NULL
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?;
        }

        Ok(())
    }

    async fn deactivate(
        &self,
        address: &str,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<bool> {
        let result = if let Some(token) = token_address {
            sqlx::query(
                r#"
                UPDATE watched_addresses
                SET is_active = FALSE
                WHERE LOWER(address) = LOWER($1) AND chain_id = $2 AND LOWER(token_address) = LOWER($3) AND is_active = TRUE
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .bind(token)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
        } else {
            sqlx::query(
                r#"
                UPDATE watched_addresses
                SET is_active = FALSE
                WHERE LOWER(address) = LOWER($1) AND chain_id = $2 AND token_address IS NULL AND is_active = TRUE
                "#,
            )
            .bind(address)
            .bind(chain_id.as_str())
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
        };

        Ok(result.rows_affected() > 0)
    }

    async fn deactivate_for_payment_option(
        &self,
        payment_option_id: &PaymentOptionId,
    ) -> RepositoryResult<u64> {
        let result = sqlx::query(
            r#"
            UPDATE watched_addresses
            SET is_active = FALSE
            WHERE payment_option_id = $1 AND is_active = TRUE
            "#,
        )
        .bind(payment_option_id.0)
        .execute(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(result.rows_affected())
    }
}

/// Legacy type alias for backwards compatibility.
/// Use `PendingWatchInfo` from the types crate instead.
pub type PendingWatch = PendingWatchInfo;
