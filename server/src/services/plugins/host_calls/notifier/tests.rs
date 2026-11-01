#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use payserver_plugin_api::PluginId;
use payserver_plugin_host::PluginHostCalls;
use types::UserId;
use uuid::Uuid;

use super::super::{DeferredCapabilities, PluginCalls};
use super::DeferredNotifier;
use crate::services::email::AccountNotice;
use crate::services::plugins::AccountNotifier;
use crate::services::plugins::pools::PluginPools;

/// Records every call it is given instead of delivering anything, and
/// answers however it is told to - so a test can see exactly what the
/// import handed it: the account id and the notice, and nothing else. The
/// trait signature itself is what proves no address ever reaches here.
#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<(UserId, AccountNotice)>>,
    fail: Option<&'static str>,
}

#[async_trait]
impl AccountNotifier for Recording {
    async fn notify_account(
        &self,
        account_id: UserId,
        notice: &AccountNotice,
    ) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push((account_id, notice.clone()));
        match self.fail {
            Some(reason) => Err(reason.to_string()),
            None => Ok(()),
        }
    }
}

fn calls(notifier: Option<Arc<Recording>>) -> (PluginCalls, DeferredNotifier) {
    let cell = DeferredNotifier::new();
    if let Some(notifier) = notifier {
        assert!(cell.publish(notifier));
    }
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    let plugin_calls = PluginCalls::new(PluginId::new("cash.random.notifier").unwrap(), pools)
        .with_capabilities(&DeferredCapabilities {
            notifier: cell.clone(),
            ..DeferredCapabilities::default()
        });
    (plugin_calls, cell)
}

/// The whole point of the capability: a plugin names an account, a subject
/// and a body, and the published notifier receives exactly that - never an
/// address, which the plugin never had to give.
#[test]
fn a_published_notifier_is_told_the_account_and_the_notice_not_an_address() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let recording = Arc::new(Recording::default());
    let (calls, _cell) = calls(Some(recording.clone()));
    let account = UserId::new();

    let answer = PluginHostCalls::account_notice(
        &calls,
        format!(
            r#"{{"account_id":"{}","subject":"heads up","body":"your plan needs attention"}}"#,
            account.0
        )
        .as_bytes(),
    );
    assert!(answer.is_ok(), "{answer:?}");

    let seen = recording.calls.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, account);
    assert_eq!(seen[0].1.subject, "heads up");
    assert_eq!(seen[0].1.body, "your plan needs attention");
}

/// Delivery failing - including the ordinary "no channel for this account"
/// case `AccountNotifier::notify_account` reports as an error - must reach
/// the plugin as an error, never as a quiet success.
#[test]
fn a_failing_notifier_is_an_error_not_a_silent_ok() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let recording = Arc::new(Recording {
        fail: Some("this account has no email on file"),
        ..Recording::default()
    });
    let (calls, _cell) = calls(Some(recording));

    let err = PluginHostCalls::account_notice(
        &calls,
        format!(
            r#"{{"account_id":"{}","subject":"s","body":"b"}}"#,
            Uuid::new_v4()
        )
        .as_bytes(),
    )
    .unwrap_err();
    assert!(err.contains("no email on file"), "{err}");
}

/// Unpublished is an error, never a quiet no-op a plugin would read as
/// "delivered".
#[test]
fn an_unpublished_notifier_is_an_error() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let (calls, _cell) = calls(None);
    let err = PluginHostCalls::account_notice(
        &calls,
        format!(
            r#"{{"account_id":"{}","subject":"s","body":"b"}}"#,
            Uuid::new_v4()
        )
        .as_bytes(),
    )
    .unwrap_err();
    assert!(err.contains("does not notify accounts"), "{err}");
}

#[test]
fn an_account_id_that_is_not_an_account_id_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let (calls, _cell) = calls(Some(Arc::new(Recording::default())));
    let err = PluginHostCalls::account_notice(
        &calls,
        br#"{"account_id":"nope","subject":"s","body":"b"}"#,
    )
    .unwrap_err();
    assert!(err.contains("is not an account id"), "{err}");
}
