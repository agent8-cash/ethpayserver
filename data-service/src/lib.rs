//! Data access layer for EthPayServer.
//!
//! This crate provides database implementations for the repository traits
//! defined in the `types` crate.
//!
//! # Features
//!
//! - `postgres` (default) - PostgreSQL implementation
//! - `test-utils` - In-memory implementation for testing
//!
//! # Migrations
//!
//! Each database implementation has its own migrations directory:
//!
//! ```bash
//! # PostgreSQL
//! sqlx migrate run --source migrations/postgres
//! ```

pub mod account_deletion;
pub mod account_standing;
pub mod analytics;
pub mod chain_cursor;
pub mod email_change;
pub mod installed_plugins;
pub mod invoice_creation;
pub mod merchant_directory;
pub mod payment_tx_index;
pub mod payout_claims;
pub mod reorg;
pub mod settlement_tolerance;
pub mod store_creation;
pub mod store_invites;
pub mod watch_reconciliation;
pub mod webhook_delivery;
pub mod webhook_outbox;

pub use account_deletion::{AccountDeletionBlockers, AccountDeletionReader};
pub use account_standing::{
    AccountStanding, AccountStandingStore, ApplyOutcome, DEFAULT_STANDING_MAX_AGE_DAYS,
    FAIL_OPEN_COUNTER, HeldStanding, StandingDecision, decide_and_surface,
};
pub use analytics::{PaymentAnalyticsReader, PaymentVolumeBucket, PaymentVolumeQuery};
pub use chain_cursor::{ChainCursor, ChainCursorReader, ChainCursorWriter};
pub use email_change::{EmailChangeRequest, EmailChangeWriter};
pub use installed_plugins::{
    InstalledPlugin, InstalledPluginReader, InstalledPluginWriter, NewInstalledPlugin,
    NewPluginEvent, PluginEvent, PluginEventKind,
};
pub use invoice_creation::InvoiceCreationWriter;
pub use merchant_directory::{MerchantAccount, MerchantDirectoryReader, MerchantStore};
pub use payment_tx_index::{PaymentTxIndexReader, PaymentTxIndexWriter};
pub use payout_claims::PayoutClaimReader;
pub use reorg::{ReorgCandidateReader, ReorgWriter};
pub use settlement_tolerance::{
    DEFAULT_TOLERANCE_PERCENT, MAX_TOLERANCE_PERCENT, SettlementAllowance,
    SettlementToleranceReader, SettlementToleranceWriter,
};
pub use store_invites::{InviteAcceptance, StoreInviteWriter};
pub use watch_reconciliation::{WatchKey, WatchReconciliation, reconcile};
pub use webhook_delivery::{
    UpsertDeliveryParams, WebhookDeliveryData, WebhookDeliveryReader, WebhookDeliveryStatus,
    WebhookDeliveryWriter,
};
pub use webhook_outbox::{WebhookObligation, WebhookOutboxReader, WebhookOutboxWriter};

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub use postgres::{
    ApiKeyAuthInfo, ApiKeyFullInfo, ApiKeyRateLimitInfo, ExpectedWatch, PendingWatch,
    PgDataService, WalletReauthChallenge, WalletRotation,
};

#[cfg(feature = "redis")]
pub mod redis;

#[cfg(feature = "redis")]
pub use redis::RedisDataService;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::InMemoryDataService;

#[cfg(all(any(test, feature = "test-utils"), feature = "postgres"))]
pub mod test_support;

// Re-export repository traits and types from the types crate for convenience.
pub use types::{
    // Watched Address (for PostgreSQL persistence)
    CleanupAddressInfo,
    // Combined traits
    DataService,
    DataServiceReader,
    DataServiceWriter,
    // Invoice
    InvoiceQueryParams,
    InvoiceReader,
    InvoiceRepository,
    InvoiceWriter,
    // Live Watched Address (for evmmonitor/Redis)
    LiveWatchedAddressReader,
    LiveWatchedAddressRepository,
    LiveWatchedAddressWriter,
    // Payment Event
    PaymentEventWriter,
    // Payment Option
    PaymentMethodId,
    PaymentOptionData,
    PaymentOptionId,
    PaymentOptionReader,
    PaymentOptionRepository,
    PaymentOptionWriter,
    // Payment
    PaymentQueryParams,
    PaymentReader,
    PaymentRepository,
    PaymentWriter,
    // Payout
    PayoutData,
    PayoutReader,
    PayoutRepository,
    PayoutStatus,
    PayoutWriter,
    PendingWatchInfo,
    // Refund
    RefundData,
    RefundReader,
    RefundRepository,
    RefundStatus,
    RefundWriter,
    // Errors
    RepositoryError,
    RepositoryResult,
    // Store Payment Method
    StorePaymentMethod,
    StorePaymentMethodReader,
    StorePaymentMethodRepository,
    StorePaymentMethodWriter,
    // Store Settings
    StoreSettings,
    StoreSettingsReader,
    StoreSettingsRepository,
    StoreSettingsWriter,
    // Store Token Policy
    StoreTokenPolicyEntry,
    StoreTokenPolicyReader,
    StoreTokenPolicyRepository,
    StoreTokenPolicyWithEntries,
    StoreTokenPolicyWriter,
    // Store Webhook
    StoreWebhook,
    StoreWebhookReader,
    StoreWebhookRepository,
    StoreWebhookWriter,
    // Token
    TokenData,
    TokenPolicyEntryInput,
    TokenPolicyMode,
    TokenQueryParams,
    TokenReader,
    TokenRepository,
    TokenWriter,
    // Account Wallet
    Wallet,
    WalletReader,
    WalletRepository,
    WalletWriter,
    WatchedAddressReader,
    WatchedAddressRepository,
    WatchedAddressWriter,
};

/// Convert sqlx::Error to RepositoryError.
///
/// This helper is needed because we can't implement From trait due to orphan rules.
#[cfg(feature = "postgres")]
pub fn sqlx_to_repo_error(e: sqlx::Error) -> RepositoryError {
    match e {
        sqlx::Error::RowNotFound => RepositoryError::NotFound("row not found".into()),
        _ => RepositoryError::Database(e.to_string()),
    }
}

#[cfg(test)]
mod tests;
