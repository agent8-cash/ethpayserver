//! The import through a real compiled wasm module and the real host linker,
//! not a Rust-level call to the implementation: a plugin built against the
//! import loads, reads a standing the store holds, and has no import by which
//! to write one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use data_service::{AccountStanding, AccountStandingStore, ApplyOutcome, HeldStanding};
use metrics_exporter_prometheus::PrometheusBuilder;
use payserver_plugin_api::PluginId;
use payserver_plugin_host::PluginEngine;
use types::RepositoryResult;
use uuid::Uuid;

use super::super::{DeferredCapabilities, PluginCalls};
use super::DeferredStanding;
use super::tests::Buf;
use crate::services::plugins::pools::PluginPools;

/// A plugin that forwards its argument to `import` and returns the answer.
fn plugin_importing(import: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"
        (module
            (import "ethpayserver" "{import}" (func $query (param i32 i32) (result i64)))
            (import "ethpayserver" "host_take" (func $take (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (global $next (mut i32) (i32.const 1024))
            (func $alloc (export "alloc") (param $len i32) (result i32)
                (local $ptr i32)
                (local.set $ptr (global.get $next))
                (global.set $next (i32.add (global.get $next) (local.get $len)))
                (local.get $ptr))
            (func (export "call") (param $ptr i32) (param $len i32) (result i64)
                (local $n i32) (local $dest i32)
                (local.set $n (i32.wrap_i64 (call $query (local.get $ptr) (local.get $len))))
                (if (i32.lt_s (local.get $n) (i32.const 0)) (then (return (i64.const 0))))
                (local.set $dest (call $alloc (local.get $n)))
                (drop (call $take (local.get $dest) (local.get $n)))
                (i64.or (i64.shl (i64.extend_i32_u (local.get $dest)) (i64.const 32))
                        (i64.extend_i32_u (local.get $n)))))
        "#
    ))
    .unwrap()
}

/// Holds one standing. Its write path panics, so a plugin reaching it would
/// fail the test rather than quietly succeed.
struct Store(HeldStanding);

#[async_trait]
impl AccountStandingStore for Store {
    async fn apply_account_standing(&self, _: &AccountStanding) -> RepositoryResult<ApplyOutcome> {
        panic!("a plugin must never be able to write a standing");
    }

    async fn get_account_standing(&self, _: Uuid) -> RepositoryResult<Option<HeldStanding>> {
        Ok(Some(self.0.clone()))
    }
}

#[test]
fn a_plugin_importing_account_standing_loads_and_reads_the_stored_standing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let account = Uuid::new_v4();
    let standing = DeferredStanding::new();
    assert!(standing.publish(Arc::new(Store(HeldStanding {
        standing: AccountStanding {
            account_id: account,
            version: 9,
            in_good_standing: false,
            paid_through: None,
            plan_name: "p".into(),
            checkout_url: Some("https://x.example/c".into()),
        },
        last_heard_at: Utc::now() - Duration::days(1),
    }))));
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    let calls = PluginCalls::new(PluginId::new("cash.random.standing").unwrap(), pools)
        .with_capabilities(&DeferredCapabilities {
            standing,
            ..DeferredCapabilities::default()
        });

    let engine = PluginEngine::new();
    let module = engine
        .compile(&plugin_importing("account_standing"))
        .unwrap();
    let mut instance = engine
        .instantiate_with_calls(&module, Arc::new(calls))
        .expect("a plugin built against account_standing must load on this host");
    let answer = instance
        .call_raw(
            "call",
            format!(r#"{{"account_id":"{account}"}}"#).as_bytes(),
            10_000_000,
        )
        .unwrap();

    let answer: serde_json::Value = serde_json::from_slice(&answer).unwrap();
    assert_eq!(answer["standing"]["version"], 9);
    assert_eq!(answer["standing"]["in_good_standing"], false);
    assert_eq!(answer["standing"]["checkout_url"], "https://x.example/c");
}

/// Loads `import` through the same host calls the positive test uses, so a
/// refusal can only be about the import itself.
fn load(import: &str) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    let calls = PluginCalls::new(PluginId::new("cash.random.standing").unwrap(), pools);
    let engine = PluginEngine::new();
    let module = engine.compile(&plugin_importing(import)).unwrap();
    engine
        .instantiate_with_calls(&module, Arc::new(calls))
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn no_import_exists_by_which_a_plugin_could_write_a_standing() {
    // The harness loads the read import, so a refusal below is about the name.
    load("account_standing").expect("the read import must load");
    for import in [
        "apply_account_standing",
        "set_account_standing",
        "account_standing_set",
    ] {
        let err = load(import).expect_err(&format!("{import} must not exist on the host"));
        assert!(err.contains(import), "refused for another reason: {err}");
    }
}

#[test]
fn a_plugin_reading_a_stale_standing_surfaces_the_fail_open() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let account = Uuid::new_v4();
    let standing = DeferredStanding::new();
    assert!(standing.publish(Arc::new(Store(HeldStanding {
        standing: AccountStanding {
            account_id: account,
            version: 1,
            in_good_standing: true,
            paid_through: None,
            plan_name: "p".into(),
            checkout_url: None,
        },
        last_heard_at: Utc::now() - Duration::days(365),
    }))));
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    let calls = PluginCalls::new(PluginId::new("cash.random.standing").unwrap(), pools)
        .with_capabilities(&DeferredCapabilities {
            standing,
            ..DeferredCapabilities::default()
        });
    let engine = PluginEngine::new();
    let module = engine
        .compile(&plugin_importing("account_standing"))
        .unwrap();
    let mut instance = engine
        .instantiate_with_calls(&module, Arc::new(calls))
        .unwrap();

    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        metrics::with_local_recorder(&recorder, || {
            instance
                .call_raw(
                    "call",
                    format!(r#"{{"account_id":"{account}"}}"#).as_bytes(),
                    10_000_000,
                )
                .unwrap()
        })
    });

    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged
            .lines()
            .any(|l| l.contains("ERROR") && l.contains("standing fail-open")),
        "no error-level event: {logged:?}"
    );
    assert!(
        handle.render().contains(&format!(
            "{}{{reason=\"stale\"}} 1",
            data_service::FAIL_OPEN_COUNTER
        )),
        "counter not incremented"
    );
}
