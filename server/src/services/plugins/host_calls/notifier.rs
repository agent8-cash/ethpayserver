use std::sync::Arc;

use crate::services::email::AccountNotice;
use crate::services::plugins::AccountNotifier;
use types::UserId;

use super::PluginCalls;

/// Late-binding cell for capability 7 (`account_notice`), separate from the
/// others because building the capability needs the full `PgAppState` - not
/// ready at plugin-registration time, the same reason `issuer` is a cell
/// rather than a constructor argument.
#[derive(Clone, Default)]
pub struct DeferredNotifier(Arc<std::sync::OnceLock<Arc<dyn AccountNotifier>>>);

impl DeferredNotifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the notifier. Returns whether this call is the one that set it.
    pub fn publish(&self, notifier: Arc<dyn AccountNotifier>) -> bool {
        self.0.set(notifier).is_ok()
    }

    fn get(&self) -> Option<&Arc<dyn AccountNotifier>> {
        self.0.get()
    }
}

impl std::fmt::Debug for DeferredNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DeferredNotifier")
            .field(&self.get().is_some())
            .finish()
    }
}

/// A plugin asking the host to tell one account something. No address here,
/// checked or otherwise - see the module doc on `account_notice` for why.
#[derive(Debug, serde::Deserialize)]
struct NoticeRequest {
    account_id: String,
    subject: String,
    body: String,
}

impl PluginCalls {
    pub(super) fn account_notice_impl(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        let parsed: NoticeRequest = serde_json::from_slice(request)
            .map_err(|e| format!("could not read the notice request: {e}"))?;
        let account_id = uuid::Uuid::parse_str(&parsed.account_id)
            .map(UserId)
            .map_err(|_| format!("{} is not an account id", parsed.account_id))?;

        let Some(notifier) = self.notifier.get().cloned() else {
            return Err("this host does not notify accounts".to_string());
        };

        let notice = AccountNotice {
            subject: parsed.subject,
            body: parsed.body,
        };

        self.handle
            .block_on(notifier.notify_account(account_id, &notice))?;

        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests;
