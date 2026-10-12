use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use data_service::{AccountStandingStore, HeldStanding, decide_and_surface};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::PluginCalls;

/// Late-binding cell for capability 7, separate from the others because it
/// needs only the data layer and is published whether or not the instance has
/// a store of its own.
#[derive(Clone, Default)]
pub struct DeferredStanding(Arc<std::sync::OnceLock<Arc<dyn AccountStandingStore>>>);

impl DeferredStanding {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the store. Returns whether this call is the one that set it.
    pub fn publish(&self, store: Arc<dyn AccountStandingStore>) -> bool {
        self.0.set(store).is_ok()
    }

    fn get(&self) -> Option<&Arc<dyn AccountStandingStore>> {
        self.0.get()
    }
}

impl std::fmt::Debug for DeferredStanding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DeferredStanding")
            .field(&self.get().is_some())
            .finish()
    }
}

/// How long the sender may stay silent before a read is reported as resting on
/// old information. The same bound the data layer defaults to.
fn max_age() -> Duration {
    Duration::days(data_service::DEFAULT_STANDING_MAX_AGE_DAYS)
}

/// A plugin asking for one account's standing. One account, never a list.
#[derive(Debug, Deserialize)]
struct StandingRequest {
    account_id: String,
}

/// The answer: the stored standing, or `null` when none was ever received.
#[derive(Debug, Serialize)]
struct StandingAnswer {
    standing: Option<StandingView>,
}

#[derive(Debug, Serialize)]
struct StandingView {
    version: i64,
    in_good_standing: bool,
    paid_through: Option<DateTime<Utc>>,
    plan_name: String,
    checkout_url: Option<String>,
    /// When the sender last confirmed this standing, so a plugin can tell a
    /// current answer from an old one.
    last_heard_at: DateTime<Utc>,
}

impl From<&HeldStanding> for StandingView {
    fn from(held: &HeldStanding) -> Self {
        Self {
            version: held.standing.version,
            in_good_standing: held.standing.in_good_standing,
            paid_through: held.standing.paid_through,
            plan_name: held.standing.plan_name.clone(),
            checkout_url: held.standing.checkout_url.clone(),
            last_heard_at: held.last_heard_at,
        }
    }
}

impl PluginCalls {
    /// Read-only: the only store method reached from here is
    /// `get_account_standing`.
    pub(super) fn account_standing_impl(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        let parsed: StandingRequest = serde_json::from_slice(request)
            .map_err(|e| format!("could not read the standing request: {e}"))?;
        let account_id = Uuid::parse_str(&parsed.account_id)
            .map_err(|_| format!("{} is not an account id", parsed.account_id))?;

        let Some(store) = self.standing.get().cloned() else {
            return Err("this host does not report account standing".to_string());
        };

        let held = self
            .handle
            .block_on(store.get_account_standing(account_id))
            .map_err(|e| format!("could not read the account standing: {e}"))?;

        // This read is the decision's real caller. An allow that rests on a
        // missing or old standing is surfaced here, where it happens, rather
        // than trusting the plugin that receives the answer to say so.
        decide_and_surface(account_id, held.as_ref(), Utc::now(), max_age());

        serde_json::to_vec(&StandingAnswer {
            standing: held.as_ref().map(StandingView::from),
        })
        .map_err(|e| format!("could not serialise the standing answer: {e}"))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wasm_tests;
