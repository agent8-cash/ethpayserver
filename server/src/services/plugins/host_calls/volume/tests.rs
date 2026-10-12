#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use payserver_plugin_api::PluginId;
use payserver_plugin_host::PluginHostCalls;

use super::super::{DeferredBulkVolume, DeferredCapabilities, DeferredIssuer};
use super::*;
use crate::services::plugins::pools::PluginPools;

/// A double that answers with a fixed volume and records what it was
/// asked, so the test can check the request survived the JSON boundary.
struct FixedVolume {
    asked: std::sync::Mutex<Vec<(types::UserId, u32, String)>>,
}

#[async_trait::async_trait]
impl crate::services::plugins::MerchantVolumeReader for FixedVolume {
    async fn merchant_volume(
        &self,
        account_id: types::UserId,
        window_days: u32,
        currency: &str,
    ) -> Result<crate::services::plugins::MerchantVolume, String> {
        self.asked
            .lock()
            .unwrap()
            .push((account_id, window_days, currency.to_string()));
        Ok(crate::services::plugins::MerchantVolume {
            volume: "12345.67".to_string(),
            currency: currency.to_string(),
            unpriced_assets: vec!["FOO".to_string()],
        })
    }
}

fn volume_calls(volume: &DeferredVolume) -> PluginCalls {
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    PluginCalls::new(PluginId::new("cash.random.volume").unwrap(), pools).with_capabilities(
        &DeferredCapabilities {
            issuer: DeferredIssuer::default(),
            volume: volume.clone(),
            bulk_volume: DeferredBulkVolume::default(),
            standing: super::super::DeferredStanding::default(),
        },
    )
}

/// The whole point of the capability, through the boundary a plugin
/// actually reaches it by: JSON in, JSON out, and the reader published
/// through the deferred cell rather than held directly.
#[test]
fn a_published_volume_reader_answers_a_plugin_in_its_own_units() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let reader = Arc::new(FixedVolume {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let volume = DeferredVolume::new();
    assert!(volume.publish(reader.clone()));
    let calls = volume_calls(&volume);

    let account = types::UserId::new();
    let answer = PluginHostCalls::merchant_volume(
        &calls,
        format!(
            r#"{{"account_id":"{}","window_days":30,"currency":"USD"}}"#,
            account.0
        )
        .as_bytes(),
    )
    .unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&answer).unwrap();
    assert_eq!(parsed["volume"], "12345.67");
    assert_eq!(parsed["currency"], "USD");
    assert_eq!(parsed["unpriced_assets"][0], "FOO");

    let asked = reader.asked.lock().unwrap();
    assert_eq!(
        asked[0],
        (account, 30, "USD".to_string()),
        "the reader must see the account, window and currency the plugin asked for"
    );
}

/// Absent means absent. A plugin that prices on volume and is handed a
/// zero would read it as "this merchant sold nothing" and bill them the
/// bottom bracket forever, which is the one wrong answer that looks
/// entirely normal.
#[test]
fn an_unpublished_volume_reader_is_an_error_and_never_a_zero() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let calls = volume_calls(&DeferredVolume::new());
    let err = PluginHostCalls::merchant_volume(
        &calls,
        format!(
            r#"{{"account_id":"{}","window_days":30}}"#,
            types::UserId::new().0
        )
        .as_bytes(),
    )
    .unwrap_err();

    assert!(err.contains("does not report merchant volume"), "{err}");
}

/// The account is parsed before it reaches a query. A plugin holds the
/// string the host handed it; anything else is a plugin asking about
/// something it made up.
#[test]
fn an_account_id_that_is_not_an_account_id_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let reader = Arc::new(FixedVolume {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let volume = DeferredVolume::new();
    volume.publish(reader.clone());
    let calls = volume_calls(&volume);

    let err = PluginHostCalls::merchant_volume(
        &calls,
        br#"{"account_id":"'; DROP TABLE subscriptions; --","window_days":30}"#,
    )
    .unwrap_err();

    assert!(err.contains("is not an account id"), "{err}");
    assert!(
        reader.asked.lock().unwrap().is_empty(),
        "a request that names no real account must not reach the reader"
    );
}

/// A plugin that omits the currency gets the instance's own unit, not an
/// empty string that would make the answer unreadable.
#[test]
fn an_omitted_currency_falls_back_to_the_instance_unit() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let reader = Arc::new(FixedVolume {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let volume = DeferredVolume::new();
    volume.publish(reader.clone());
    let calls = volume_calls(&volume);

    PluginHostCalls::merchant_volume(
        &calls,
        format!(
            r#"{{"account_id":"{}","window_days":30,"currency":"  "}}"#,
            types::UserId::new().0
        )
        .as_bytes(),
    )
    .unwrap();

    assert_eq!(reader.asked.lock().unwrap()[0].2, DEFAULT_VOLUME_CURRENCY);
}

fn bulk_volume_calls(bulk_volume: &DeferredBulkVolume) -> PluginCalls {
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    PluginCalls::new(PluginId::new("cash.random.bulkvolume").unwrap(), pools).with_capabilities(
        &DeferredCapabilities {
            issuer: DeferredIssuer::default(),
            volume: DeferredVolume::default(),
            bulk_volume: bulk_volume.clone(),
            standing: super::super::DeferredStanding::default(),
        },
    )
}
/// A double that answers a fixed volume for every account it is asked
/// about, and records the batch it was asked for.
struct FixedBulkVolume {
    asked: std::sync::Mutex<Vec<(Vec<types::UserId>, u32, String)>>,
}

#[async_trait::async_trait]
impl super::super::super::BulkMerchantVolumeReader for FixedBulkVolume {
    async fn merchant_volumes(
        &self,
        account_ids: &[types::UserId],
        window_days: u32,
        currency: &str,
    ) -> Result<Vec<super::super::super::AccountVolume>, String> {
        self.asked
            .lock()
            .unwrap()
            .push((account_ids.to_vec(), window_days, currency.to_string()));
        Ok(account_ids
            .iter()
            .map(|&account_id| super::super::super::AccountVolume {
                account_id,
                volume: super::super::super::MerchantVolume {
                    volume: "12345.67".to_string(),
                    currency: currency.to_string(),
                    unpriced_assets: vec!["FOO".to_string()],
                },
            })
            .collect())
    }
}

/// The batched form, through the same boundary: JSON in, JSON out, one
/// entry per requested account.
#[test]
fn a_published_bulk_volume_reader_answers_a_plugin_in_its_own_units() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let reader = Arc::new(FixedBulkVolume {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let bulk_volume = DeferredBulkVolume::new();
    assert!(bulk_volume.publish(reader.clone()));
    let calls = bulk_volume_calls(&bulk_volume);

    let accounts = [types::UserId::new(), types::UserId::new()];
    let answer = PluginHostCalls::merchant_volumes(
        &calls,
        format!(
            r#"{{"account_ids":["{}","{}"],"window_days":30,"currency":"USD"}}"#,
            accounts[0].0, accounts[1].0
        )
        .as_bytes(),
    )
    .unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&answer).unwrap();
    let entries = parsed["accounts"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["volume"], "12345.67");
    assert_eq!(entries[0]["currency"], "USD");
    assert_eq!(entries[0]["unpriced_assets"][0], "FOO");

    let asked = reader.asked.lock().unwrap();
    assert_eq!(
        asked[0],
        (accounts.to_vec(), 30, "USD".to_string()),
        "the reader must see the whole batch the plugin asked for"
    );
}

/// Unpublished must fail the same way the single-account form does: an
/// error, not an empty list that would read as "nobody sold anything".
#[test]
fn an_unpublished_bulk_volume_reader_is_an_error_and_never_empty() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let calls = bulk_volume_calls(&DeferredBulkVolume::new());
    let err = PluginHostCalls::merchant_volumes(
        &calls,
        format!(
            r#"{{"account_ids":["{}"],"window_days":30}}"#,
            types::UserId::new().0
        )
        .as_bytes(),
    )
    .unwrap_err();

    assert!(err.contains("does not report merchant volume"), "{err}");
}

/// Same rule as the single-account form: every id is parsed before any
/// of them reaches a query, so one made-up id refuses the whole batch.
#[test]
fn a_bulk_request_with_one_bad_account_id_is_refused_entirely() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let reader = Arc::new(FixedBulkVolume {
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let bulk_volume = DeferredBulkVolume::new();
    bulk_volume.publish(reader.clone());
    let calls = bulk_volume_calls(&bulk_volume);

    let err = PluginHostCalls::merchant_volumes(
        &calls,
        format!(
            r#"{{"account_ids":["{}","not-an-id"],"window_days":30}}"#,
            types::UserId::new().0
        )
        .as_bytes(),
    )
    .unwrap_err();

    assert!(err.contains("is not an account id"), "{err}");
    assert!(
        reader.asked.lock().unwrap().is_empty(),
        "a batch naming one made-up account must not reach the reader at all"
    );
}
