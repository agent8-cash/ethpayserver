//! PostgreSQL implementation of receipt delivery tracking

use async_trait::async_trait;
use sqlx::Row;
use types::InvoiceId;
use uuid::Uuid;

use super::PgDataService;
use crate::{
    ReceiptDeliveryReader, ReceiptDeliveryRecord, ReceiptDeliveryStatus, ReceiptDeliveryWriter,
    UpsertReceiptDeliveryParams, sqlx_to_repo_error,
};

#[async_trait]
impl ReceiptDeliveryReader for PgDataService {
    async fn get_last_delivery(
        &self,
        invoice_id: &InvoiceId,
    ) -> Result<Option<ReceiptDeliveryRecord>, Box<dyn std::error::Error + Send + Sync>> {
        let row = sqlx::query(
            "SELECT id, invoice_id, recipient_email, status, error_message, attempted_at, updated_at \
             FROM receipt_delivery_log \
             WHERE invoice_id = $1 \
             ORDER BY attempted_at DESC \
             LIMIT 1",
        )
        .bind(invoice_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.map(|r| {
            let status_str: String = r.get("status");
            ReceiptDeliveryRecord {
                id: r.get("id"),
                invoice_id: r.get("invoice_id"),
                recipient_email: r.get("recipient_email"),
                status: match status_str.as_str() {
                    "sent" => ReceiptDeliveryStatus::Sent,
                    "bounced" => ReceiptDeliveryStatus::Bounced,
                    "failed" => ReceiptDeliveryStatus::Failed,
                    _ => ReceiptDeliveryStatus::Pending,
                },
                error_message: r.get("error_message"),
                attempted_at: r.get("attempted_at"),
                updated_at: r.get("updated_at"),
            }
        }))
    }

    async fn get_all_deliveries(
        &self,
        invoice_id: &InvoiceId,
    ) -> Result<Vec<ReceiptDeliveryRecord>, Box<dyn std::error::Error + Send + Sync>> {
        let rows = sqlx::query(
            "SELECT id, invoice_id, recipient_email, status, error_message, attempted_at, updated_at \
             FROM receipt_delivery_log \
             WHERE invoice_id = $1 \
             ORDER BY attempted_at DESC",
        )
        .bind(invoice_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let status_str: String = r.get("status");
                ReceiptDeliveryRecord {
                    id: r.get("id"),
                    invoice_id: r.get("invoice_id"),
                    recipient_email: r.get("recipient_email"),
                    status: match status_str.as_str() {
                        "sent" => ReceiptDeliveryStatus::Sent,
                        "bounced" => ReceiptDeliveryStatus::Bounced,
                        "failed" => ReceiptDeliveryStatus::Failed,
                        _ => ReceiptDeliveryStatus::Pending,
                    },
                    error_message: r.get("error_message"),
                    attempted_at: r.get("attempted_at"),
                    updated_at: r.get("updated_at"),
                }
            })
            .collect())
    }

    async fn is_delivered(
        &self,
        invoice_id: &InvoiceId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let row = sqlx::query(
            "SELECT EXISTS(
                SELECT 1 FROM receipt_delivery_log \
                WHERE invoice_id = $1 AND status = 'sent'
            ) AS is_delivered",
        )
        .bind(invoice_id.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.get("is_delivered"))
    }

    async fn count_bounced_for_store(
        &self,
        store_id: Uuid,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let row = sqlx::query(
            "SELECT COUNT(*) as count FROM receipt_delivery_log \
             JOIN invoices ON receipt_delivery_log.invoice_id = invoices.id \
             WHERE invoices.store_id = $1 AND receipt_delivery_log.status = 'bounced'",
        )
        .bind(store_id)
        .fetch_one(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.get::<i64, _>("count"))
    }
}

#[async_trait]
impl ReceiptDeliveryWriter for PgDataService {
    async fn upsert_delivery(
        &self,
        params: UpsertReceiptDeliveryParams,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        sqlx::query(
            "INSERT INTO receipt_delivery_log \
             (invoice_id, recipient_email, status, error_message) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&params.invoice_id)
        .bind(&params.recipient_email)
        .bind(params.status.as_str())
        .bind(&params.error_message)
        .execute(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(())
    }

    async fn mark_bounced(
        &self,
        invoice_id: &InvoiceId,
        recipient_email: &str,
        bounce_reason: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        sqlx::query(
            "UPDATE receipt_delivery_log \
             SET status = 'bounced', error_message = $3, updated_at = NOW() \
             WHERE invoice_id = $1 AND recipient_email = $2",
        )
        .bind(invoice_id.as_str())
        .bind(recipient_email)
        .bind(bounce_reason)
        .execute(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(())
    }
}
