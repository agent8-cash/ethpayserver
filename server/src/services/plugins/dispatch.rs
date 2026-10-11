//! The dispatch path: what turns a loaded plugin into something the rest of
//! the server actually calls.
//!
//! Every capability module beside this one defines a host-side trait and is
//! tested against a hand-written implementation. That proves the capability
//! behaves and proves nothing about whether plugin code ever runs — and until
//! this module existed, none did. `run_action` and `run_filter` had no callers
//! outside the host's own tests, and `AppState::invoice_creation_filters` was
//! populated in tests and never in the live server.
//!
//! This is the adapter layer that closes that gap: each type here implements a
//! capability trait by calling one wasm export on one plugin.
//!
//! # The wire contract
//!
//! The host defines it; a plugin matches it. Arguments and answers are JSON,
//! because [`PluginHost::run_filter`] and [`PluginHost::run_action`] are
//! generic over `serde` and the plugin ABI carries opaque bytes. The exports
//! are named by the constants below rather than inline strings, so the two
//! sides can be compared in one place.
//!
//! # Routing by declared kind
//!
//! A plugin is only offered an export its manifest says it can answer. That is
//! not tidiness: a filter that cannot run resolves to its manifest's
//! [`FailureMode`](payserver_plugin_api::FailureMode), which defaults to
//! *closed*. Register one action plugin as a filter and every invoice on the
//! instance is refused, for as long as it stays installed. [`PluginHost::kind`]
//! is what prevents that, and `only_filters` is the test that pins it.
//!
//! `cancel_subscription` below does not go through that gate, and the reason
//! is what it is dispatched *for*: the filter and payment-observer calls above
//! are broadcast to every loaded plugin of the matching kind on every invoice
//! or every settlement, so a wrong one wedges the instance until someone
//! notices. A cancellation is the opposite shape - one admin, asking one
//! plugin they picked by id, once. A plugin that does not implement the
//! export just reports it could not run, the same as a filter that trapped;
//! nothing else on the instance is affected either way.

use std::sync::Arc;

use async_trait::async_trait;
use auth::UserId;
use payserver_plugin_api::{PluginId, PluginKind};
use serde::{Deserialize, Serialize};

use super::account_closed::AccountClosedObserver;
use super::filter::{FilterVerdict, InvoiceCreationFilter, InvoiceCreationFilterRequest};
use super::payment_observer::{OwnStorePayment, OwnStorePaymentObserver};
use payserver_plugin_host::{FilterOutcome, PluginHost};

/// The export consulted before an invoice is created.
pub const FILTER_INVOICE_CREATION: &str = "filter_invoice_creation";

/// The export told that an own-store invoice settled.
pub const PAYMENT_SETTLED: &str = "payment_settled";

/// The export told that an account no longer exists.
pub const ACCOUNT_CLOSED: &str = "account_closed";

/// Shown to the merchant when a filter refuses but says nothing useful, or
/// cannot run at all and its manifest fails closed.
///
/// A plugin's own `reason` is shown verbatim, so a refusal that carries one
/// uses it. What must never reach a merchant is the *internal* reason a filter
/// could not run — "wasm trap at 0x1f4", a deadline in milliseconds — which
/// names our implementation and tells them nothing they can act on. Those are
/// logged at warn instead.
const UNAVAILABLE: &str = "Invoice creation is temporarily unavailable. Please try again shortly.";

#[derive(Debug, Serialize)]
struct WireFilterRequest {
    store_id: String,
    /// The merchant who owns the store. Billing is per merchant, so this is
    /// the key a subscription is actually looked up by.
    account_id: String,
}

#[derive(Debug, Deserialize)]
struct WireFilterVerdict {
    allow: bool,
    #[serde(default)]
    reason: Option<String>,
    /// What an allow rested on, when the plugin reports it. Held as raw JSON
    /// so a basis this host does not understand cannot turn an answered
    /// verdict into "could not run".
    #[serde(default)]
    standing_basis: Option<serde_json::Value>,
}

/// Counter incremented once per invoice-creation allow that rests on a
/// standing never received or older than the freshness bound, labelled by
/// `reason`. Unlike the per-read counter in the data layer, it is counted
/// where the outcome is known: a page render or a deny never reaches it.
pub const FAIL_OPEN_ALLOW_COUNTER: &str = "ethpayserver_standing_fail_open_allow_total";

/// Why an allow rests on missing or old information, or `None` when it rests
/// on a standing confirmed within `max_age` (or says nothing about standing).
fn fail_open_reason(
    basis: Option<&serde_json::Value>,
    now: chrono::DateTime<chrono::Utc>,
    max_age: chrono::Duration,
) -> Option<(&'static str, Option<chrono::Duration>)> {
    let basis = basis?;
    match basis.get("basis").and_then(serde_json::Value::as_str) {
        Some("never_received") => Some(("unheard", None)),
        Some("confirmed") => {
            let heard = basis
                .get("last_heard_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc));
            match heard {
                Some(heard) if now - heard <= max_age => None,
                Some(heard) => Some(("stale", Some(now - heard))),
                None => Some(("unreadable", None)),
            }
        }
        _ => Some(("unreadable", None)),
    }
}

/// Surface a fail-open allow at error level plus a counter.
fn surface_fail_open_allow(
    plugin: &PluginId,
    account_id: &str,
    basis: Option<&serde_json::Value>,
    now: chrono::DateTime<chrono::Utc>,
) {
    let max_age = chrono::Duration::days(data_service::DEFAULT_STANDING_MAX_AGE_DAYS);
    let Some((reason, silent_for)) = fail_open_reason(basis, now, max_age) else {
        return;
    };
    metrics::counter!(FAIL_OPEN_ALLOW_COUNTER, "reason" => reason).increment(1);
    tracing::error!(
        %plugin,
        %account_id,
        reason,
        silent_for_secs = silent_for.map(|d| d.num_seconds()),
        max_age_secs = max_age.num_seconds(),
        "invoice creation allowed on a standing that was never received or is older than the freshness bound"
    );
}

#[derive(Debug, Serialize)]
struct WirePaymentSettled {
    invoice_id: String,
    currency: String,
    amount_received: String,
    status: String,
    settled_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

impl From<&OwnStorePayment> for WirePaymentSettled {
    fn from(payment: &OwnStorePayment) -> Self {
        Self {
            invoice_id: payment.invoice_id.as_str().to_string(),
            currency: payment.currency.clone(),
            amount_received: payment.amount_received.clone(),
            status: payment.status.to_string(),
            settled_at: payment.settled_at.to_rfc3339(),
            metadata: payment.metadata.clone(),
        }
    }
}

/// One plugin, asked whether an invoice may be created.
pub struct PluginInvoiceCreationFilter {
    host: Arc<PluginHost>,
    id: PluginId,
}

impl PluginInvoiceCreationFilter {
    #[must_use]
    pub fn new(host: Arc<PluginHost>, id: PluginId) -> Self {
        Self { host, id }
    }
}

#[async_trait]
impl InvoiceCreationFilter for PluginInvoiceCreationFilter {
    async fn filter_invoice_creation(
        &self,
        request: InvoiceCreationFilterRequest,
    ) -> FilterVerdict {
        let wire = WireFilterRequest {
            store_id: request.store_id.0.to_string(),
            account_id: request.account_id.0.to_string(),
        };
        let account_id = wire.account_id.clone();

        let outcome: FilterOutcome<WireFilterVerdict> = self
            .host
            .run_filter(&self.id, FILTER_INVOICE_CREATION, &wire)
            .await;

        match outcome {
            FilterOutcome::Ran(verdict) if verdict.allow => {
                surface_fail_open_allow(
                    &self.id,
                    &account_id,
                    verdict.standing_basis.as_ref(),
                    chrono::Utc::now(),
                );
                FilterVerdict::Allow
            }
            FilterOutcome::Ran(verdict) => FilterVerdict::Deny {
                // The plugin's own words where it gave any: this is its one
                // chance to tell the merchant what to do about it.
                reason: verdict.reason.unwrap_or_else(|| UNAVAILABLE.to_string()),
            },
            FilterOutcome::CouldNotRun {
                allowed: true,
                reason,
            } => {
                tracing::warn!(
                    plugin = %self.id,
                    %reason,
                    "invoice-creation filter could not run; its manifest fails open, so the invoice is allowed"
                );
                FilterVerdict::Allow
            }
            FilterOutcome::CouldNotRun {
                allowed: false,
                reason,
            } => {
                tracing::warn!(
                    plugin = %self.id,
                    %reason,
                    "invoice-creation filter could not run; its manifest fails closed, so the invoice is refused"
                );
                // Deliberately not `reason`: that string names our internals
                // and the merchant can act on none of it.
                FilterVerdict::Deny {
                    reason: UNAVAILABLE.to_string(),
                }
            }
        }
    }
}

/// One plugin, told that an invoice on the instance's own store settled.
pub struct PluginPaymentObserver {
    host: Arc<PluginHost>,
    id: PluginId,
}

impl PluginPaymentObserver {
    #[must_use]
    pub fn new(host: Arc<PluginHost>, id: PluginId) -> Self {
        Self { host, id }
    }
}

#[async_trait]
impl OwnStorePaymentObserver for PluginPaymentObserver {
    async fn payment_settled(&self, payment: &OwnStorePayment) {
        // `run_action` is fire-and-forget by construction: it spawns, bounds
        // the call by the host's deadline, records success or failure against
        // the plugin, and returns nothing. That is exactly the guarantee this
        // capability promises — an observer cannot delay or fail a payment.
        self.host.run_action(
            &self.id,
            PAYMENT_SETTLED,
            &WirePaymentSettled::from(payment),
        );
    }
}

/// The export asked to cancel one account's subscription now.
pub const CANCEL_SUBSCRIPTION: &str = "cancel_subscription";

#[derive(Debug, Serialize)]
struct WireCancelSubscriptionRequest<'a> {
    account_id: &'a str,
}

/// What a plugin answers a cancellation request with.
#[derive(Debug, Deserialize)]
struct WireCancelSubscriptionAnswer {
    cancelled: bool,
    /// The plugin's own words, same convention as a filter's `reason`: shown
    /// to the admin verbatim when the plugin gives one.
    #[serde(default)]
    reason: Option<String>,
}

/// What asking a plugin to cancel a subscription came back with.
#[derive(Debug, PartialEq, Eq)]
pub enum CancelSubscriptionOutcome {
    /// The plugin cancelled it.
    Cancelled,
    /// The plugin ran and declined - no such account, already cancelled,
    /// whatever `reason` says.
    Refused { reason: Option<String> },
    /// The call itself did not complete: no such plugin, it trapped, it ran
    /// past the deadline, or its answer did not parse.
    CouldNotRun { reason: String },
}

/// Ask `id` to cancel `account_id`'s subscription now.
///
/// Unlike [`run_filter`](PluginHost::run_filter), there is no failure-mode
/// fallback: an admin action that silently no-ops on an unreachable plugin
/// would report success when nothing happened. Every non-success path is
/// returned instead of papered over.
pub async fn cancel_subscription(
    host: &PluginHost,
    id: &PluginId,
    account_id: &str,
) -> CancelSubscriptionOutcome {
    let wire = WireCancelSubscriptionRequest { account_id };
    match host
        .run_query::<_, WireCancelSubscriptionAnswer>(id, CANCEL_SUBSCRIPTION, &wire)
        .await
    {
        Ok(answer) if answer.cancelled => CancelSubscriptionOutcome::Cancelled,
        Ok(answer) => CancelSubscriptionOutcome::Refused {
            reason: answer.reason,
        },
        Err(reason) => CancelSubscriptionOutcome::CouldNotRun { reason },
    }
}

/// Adapters for every loaded plugin whose manifest declares it a filter.
///
/// Plugins that declare any other kind are skipped rather than registered and
/// left to fail — see the module doc on why a mis-registered filter refuses
/// every invoice on the instance.
#[must_use]
pub fn invoice_creation_filters(
    host: &Arc<PluginHost>,
    loaded: &[PluginId],
) -> Vec<Arc<dyn InvoiceCreationFilter>> {
    loaded
        .iter()
        .filter(|id| host.kind(id).is_some_and(PluginKind::is_filter))
        .map(|id| {
            Arc::new(PluginInvoiceCreationFilter::new(
                Arc::clone(host),
                id.clone(),
            )) as Arc<dyn InvoiceCreationFilter>
        })
        .collect()
}

/// One plugin, told that an account no longer exists.
pub struct PluginAccountClosedObserver {
    host: Arc<PluginHost>,
    id: PluginId,
}

impl PluginAccountClosedObserver {
    #[must_use]
    pub fn new(host: Arc<PluginHost>, id: PluginId) -> Self {
        Self { host, id }
    }
}

#[derive(Debug, Serialize)]
struct WireAccountClosed {
    account_id: String,
}

#[async_trait]
impl AccountClosedObserver for PluginAccountClosedObserver {
    async fn account_closed(&self, account_id: UserId) {
        // Fire-and-forget, the same as `PluginPaymentObserver::payment_settled`
        // above - see that impl's comment for why `run_action` is the whole
        // guarantee this capability needs.
        self.host.run_action(
            &self.id,
            ACCOUNT_CLOSED,
            &WireAccountClosed {
                account_id: account_id.0.to_string(),
            },
        );
    }
}

/// Adapters for every loaded plugin, to be told an account was deleted.
///
/// Unfiltered by kind, for the same reason [`payment_observers`] is: an
/// action has no failure mode to resolve, so there is no "refuses everything"
/// hazard a kind check would be protecting against.
#[must_use]
pub fn account_closed_observers(
    host: &Arc<PluginHost>,
    loaded: &[PluginId],
) -> Vec<Arc<dyn AccountClosedObserver>> {
    loaded
        .iter()
        .map(|id| {
            Arc::new(PluginAccountClosedObserver::new(
                Arc::clone(host),
                id.clone(),
            )) as Arc<dyn AccountClosedObserver>
        })
        .collect()
}

/// Adapters for every loaded plugin, to be told about own-store payments.
///
/// Unfiltered by kind, unlike the filters above, and the asymmetry is
/// deliberate: a plugin that does not export `payment_settled` simply records
/// a failure and nothing else happens, because an action has no failure mode
/// to resolve and no caller waiting on an answer. There is no equivalent of
/// "refuses every invoice" to protect against here.
#[must_use]
pub fn payment_observers(
    host: &Arc<PluginHost>,
    loaded: &[PluginId],
) -> Vec<Arc<dyn OwnStorePaymentObserver>> {
    loaded
        .iter()
        .map(|id| {
            Arc::new(PluginPaymentObserver::new(Arc::clone(host), id.clone()))
                as Arc<dyn OwnStorePaymentObserver>
        })
        .collect()
}

/// Decide whether own-store payment reporting is on, given what the boot
/// found.
///
/// Both halves are required and the asymmetry matters: a configured store with
/// no plugins has nobody to notify, and observers with no configured store must
/// never be handed a guess at which store is ours — that guess is how a plugin
/// ends up reading a merchant's payments.
///
/// Extracted from `main` deliberately. The decision is three lines and lives in
/// a binary nothing can test, which is precisely the shape of the wiring bugs
/// this repo keeps finding: the capability works, the boot never switches it on,
/// and every test still passes.
#[must_use]
pub fn own_store_payment_reporting(
    operator_store_id: Option<types::StoreId>,
    observers: Vec<Arc<dyn OwnStorePaymentObserver>>,
) -> Option<(types::StoreId, Vec<Arc<dyn OwnStorePaymentObserver>>)> {
    if observers.is_empty() {
        return None;
    }
    operator_store_id.map(|store_id| (store_id, observers))
}

#[cfg(test)]
mod tests;
