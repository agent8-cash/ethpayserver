//! Receipt delivery tracking - records email delivery status for customer payment receipts

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use types::InvoiceId;
use uuid::Uuid;

/// Status of a receipt delivery attempt
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptDeliveryStatus {
    /// Not yet attempted
    Pending,
    /// Successfully sent to SMTP server
    Sent,
    /// Hard bounce (permanent delivery failure)
    Bounced,
    /// Other send failure (soft bounce, network error, etc.)
    Failed,
}

impl ReceiptDeliveryStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Bounced => "bounced",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ReceiptDeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Record of a receipt delivery attempt
#[derive(Debug, Clone)]
pub struct ReceiptDeliveryRecord {
    pub id: Uuid,
    pub invoice_id: String,
    pub recipient_email: String,
    pub status: ReceiptDeliveryStatus,
    pub error_message: Option<String>,
    pub attempted_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Data for recording a receipt delivery attempt
#[derive(Debug, Clone)]
pub struct UpsertReceiptDeliveryParams {
    pub invoice_id: String,
    pub recipient_email: String,
    pub status: ReceiptDeliveryStatus,
    pub error_message: Option<String>,
}

/// Reader trait for receipt delivery records
#[async_trait]
pub trait ReceiptDeliveryReader: Send + Sync {
    /// Get the most recent delivery attempt for an invoice
    async fn get_last_delivery(
        &self,
        invoice_id: &InvoiceId,
    ) -> Result<Option<ReceiptDeliveryRecord>, Box<dyn std::error::Error + Send + Sync>>;

    /// Get all delivery attempts for an invoice
    async fn get_all_deliveries(
        &self,
        invoice_id: &InvoiceId,
    ) -> Result<Vec<ReceiptDeliveryRecord>, Box<dyn std::error::Error + Send + Sync>>;

    /// Check if a receipt was successfully delivered
    async fn is_delivered(
        &self,
        invoice_id: &InvoiceId,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Get count of bounced receipts for a store
    async fn count_bounced_for_store(
        &self,
        store_id: Uuid,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>>;
}

/// Writer trait for recording receipt delivery attempts
#[async_trait]
pub trait ReceiptDeliveryWriter: Send + Sync {
    /// Record a receipt delivery attempt
    async fn upsert_delivery(
        &self,
        params: UpsertReceiptDeliveryParams,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Mark a receipt as bounced
    async fn mark_bounced(
        &self,
        invoice_id: &InvoiceId,
        recipient_email: &str,
        bounce_reason: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
