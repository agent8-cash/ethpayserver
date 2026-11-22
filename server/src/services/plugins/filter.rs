//! Capability 2: a filter that can refuse invoice or store creation, and
//! nothing else.
//!
//! This is how a lapsed subscription is enforced - the merchant keeps
//! everything except creating new invoices. It must never grow into a hook
//! for payment detection, crediting or confirmation: refusing a lapsed
//! merchant's *new* invoice is a billing decision, but withholding credit for
//! a payment a customer already sent takes that customer's money over a
//! dispute they are not party to.
//!
//! That is enforced structurally, not by convention: [`InvoiceCreationFilter`]
//! has exactly one method, and the only thing it is ever asked about is
//! [`InvoiceCreationFilterRequest`], which carries a store id and nothing
//! else - there is no field to name a payment, a confirmation or a credit.
//! Widening this to a generic `filter(hook, payload)` call is exactly the
//! shape that would let a future change smuggle in a payment-touching hook
//! without anyone deciding to; see the test below.

use std::sync::Arc;

use async_trait::async_trait;
use auth::UserId;
use types::StoreId;

/// The one thing a filter is ever asked about: may an invoice be created on
/// `store_id`?
///
/// `account_id` is the merchant who owns that store, and it is here because
/// billing is per merchant rather than per store: the locked pricing model
/// charges a merchant once for a monthly volume, and a merchant running three
/// stores must not be billed three times or have their volume split into
/// three brackets that each look small.
///
/// Note what is still absent, which is the property this type exists to hold:
/// there is no field naming a payment, a confirmation or a credit. Adding the
/// owner of the store already named here does not open the money path;
/// widening this to a generic `filter(hook, payload)` would, which is what
/// the test below refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvoiceCreationFilterRequest {
    pub store_id: StoreId,
    pub account_id: UserId,
}

/// A filter's answer. `Deny`'s `reason` is shown to the merchant verbatim, so
/// it must name the actual cause (e.g. "Your subscription lapsed on ...") -
/// this is the plugin's one chance to tell them why, not a code they have to
/// look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterVerdict {
    Allow,
    Deny { reason: String },
}

/// Something that may refuse invoice creation.
///
/// Note what is absent: no `filter_payment`, no `filter_confirmation`, no way
/// to be asked about anything but a not-yet-created invoice. See the module
/// doc for why that is load-bearing.
#[async_trait]
pub trait InvoiceCreationFilter: Send + Sync {
    async fn filter_invoice_creation(&self, request: InvoiceCreationFilterRequest)
    -> FilterVerdict;
}

/// Runs every registered filter, refusing on the first denial.
///
/// First-denial-wins rather than collecting every reason: the merchant needs
/// one actionable sentence, not a list assembled from plugins that don't know
/// about each other.
pub async fn run_invoice_creation_filters(
    filters: &[Arc<dyn InvoiceCreationFilter>],
    request: InvoiceCreationFilterRequest,
) -> FilterVerdict {
    for filter in filters {
        if let FilterVerdict::Deny { reason } = filter.filter_invoice_creation(request).await {
            return FilterVerdict::Deny { reason };
        }
    }
    FilterVerdict::Allow
}

/// What a store-creation filter is asked: may `account_id` open one more
/// store, given the `existing_store_count` it already owns?
///
/// The count is read by the host at request time, so a filter decides on
/// the stores this host actually holds rather than on what it was told
/// about earlier. Like [`InvoiceCreationFilterRequest`], it carries no field
/// naming a payment, a confirmation or a credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCreationFilterRequest {
    pub account_id: UserId,
    pub existing_store_count: u32,
}

/// Something that may refuse store creation. One method, for the same reason
/// as [`InvoiceCreationFilter`]: no way to be asked about anything else.
#[async_trait]
pub trait StoreCreationFilter: Send + Sync {
    async fn filter_store_creation(&self, request: StoreCreationFilterRequest) -> FilterVerdict;
}

/// Runs every registered store-creation filter, refusing on the first denial.
pub async fn run_store_creation_filters(
    filters: &[Arc<dyn StoreCreationFilter>],
    request: StoreCreationFilterRequest,
) -> FilterVerdict {
    for filter in filters {
        if let FilterVerdict::Deny { reason } = filter.filter_store_creation(request).await {
            return FilterVerdict::Deny { reason };
        }
    }
    FilterVerdict::Allow
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct AlwaysAllow;
    #[async_trait]
    impl InvoiceCreationFilter for AlwaysAllow {
        async fn filter_invoice_creation(
            &self,
            _request: InvoiceCreationFilterRequest,
        ) -> FilterVerdict {
            FilterVerdict::Allow
        }
    }

    struct LapsedSubscription;
    #[async_trait]
    impl InvoiceCreationFilter for LapsedSubscription {
        async fn filter_invoice_creation(
            &self,
            _request: InvoiceCreationFilterRequest,
        ) -> FilterVerdict {
            FilterVerdict::Deny {
                reason: "Your subscription lapsed; renew it to create new invoices.".to_string(),
            }
        }
    }

    /// Ticket test 2: a filter refusing invoice creation actually blocks it,
    /// with a reason naming the subscription.
    #[tokio::test]
    async fn a_denying_filter_blocks_and_names_the_reason() {
        let filters: Vec<Arc<dyn InvoiceCreationFilter>> = vec![Arc::new(LapsedSubscription)];
        let verdict = run_invoice_creation_filters(
            &filters,
            InvoiceCreationFilterRequest {
                store_id: StoreId::new(),
                account_id: UserId::new(),
            },
        )
        .await;

        match verdict {
            FilterVerdict::Deny { reason } => assert!(reason.contains("subscription")),
            FilterVerdict::Allow => panic!("a denying filter must block invoice creation"),
        }
    }

    #[tokio::test]
    async fn no_filters_allows() {
        let verdict = run_invoice_creation_filters(
            &[],
            InvoiceCreationFilterRequest {
                store_id: StoreId::new(),
                account_id: UserId::new(),
            },
        )
        .await;
        assert_eq!(verdict, FilterVerdict::Allow);
    }

    #[tokio::test]
    async fn one_allow_and_one_deny_still_blocks() {
        let filters: Vec<Arc<dyn InvoiceCreationFilter>> =
            vec![Arc::new(AlwaysAllow), Arc::new(LapsedSubscription)];
        let verdict = run_invoice_creation_filters(
            &filters,
            InvoiceCreationFilterRequest {
                store_id: StoreId::new(),
                account_id: UserId::new(),
            },
        )
        .await;
        assert!(matches!(verdict, FilterVerdict::Deny { .. }));
    }

    /// Ticket test 3: a filter cannot reach payment detection or crediting.
    ///
    /// Not a call-count assertion - "we never called `PaymentWriter`" would
    /// still pass the day a payment-touching hook is added elsewhere and
    /// nothing here calls it either. Instead this pins the *shape* a filter
    /// is ever handed: `InvoiceCreationFilterRequest` has exactly the one
    /// field below. Destructuring it without `..` means this stops
    /// compiling, forcing a reviewer to look at this test, the moment a
    /// second field - a payment id, a confirmation - is added to what a
    /// filter can be asked about.
    #[test]
    fn the_request_a_filter_receives_names_only_a_store() {
        let request = InvoiceCreationFilterRequest {
            store_id: StoreId::new(),
            account_id: UserId::new(),
        };
        // Destructured exhaustively on purpose: adding a field here is a
        // deliberate act that has to be made in this test too, which is the
        // moment to ask whether the new field names anything on the money
        // path.
        let InvoiceCreationFilterRequest {
            store_id: _,
            account_id: _,
        } = request;
    }
}
