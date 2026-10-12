#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use data_service::{AccountStanding, AccountStandingStore, ApplyOutcome, HeldStanding};
use metrics_exporter_prometheus::PrometheusBuilder;
use payserver_plugin_api::PluginId;
use types::{RepositoryError, RepositoryResult};
use uuid::Uuid;

use super::super::{DeferredCapabilities, PluginCalls};
use super::DeferredStanding;
use crate::services::plugins::pools::PluginPools;

/// Holds whatever it is given, and fails when told to. It has no write path a
/// test could reach: `apply_account_standing` panics, which is how the tests
/// show the import never writes.
struct Held {
    row: Option<HeldStanding>,
    fail: bool,
}

#[async_trait]
impl AccountStandingStore for Held {
    async fn apply_account_standing(&self, _: &AccountStanding) -> RepositoryResult<ApplyOutcome> {
        panic!("the account_standing import must never write a standing");
    }

    async fn get_account_standing(&self, _: Uuid) -> RepositoryResult<Option<HeldStanding>> {
        if self.fail {
            return Err(RepositoryError::Database("down".into()));
        }
        Ok(self.row.clone())
    }
}

fn held(good: bool, heard: DateTime<Utc>) -> HeldStanding {
    HeldStanding {
        standing: AccountStanding {
            account_id: Uuid::nil(),
            version: 3,
            in_good_standing: good,
            paid_through: None,
            plan_name: "p".into(),
            checkout_url: Some("https://x.example/c".into()),
        },
        last_heard_at: heard,
    }
}

fn calls(store: Option<Held>) -> PluginCalls {
    let standing = DeferredStanding::new();
    if let Some(store) = store {
        assert!(standing.publish(Arc::new(store)));
    }
    let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
    PluginCalls::new(PluginId::new("cash.random.standing").unwrap(), pools).with_capabilities(
        &DeferredCapabilities {
            standing,
            ..DeferredCapabilities::default()
        },
    )
}

#[derive(Clone, Default)]
pub(super) struct Buf(pub(super) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

/// What one import call did: its answer, the fail-open counter by reason, and
/// the error-level log lines it wrote. All read from recorders local to this
/// thread, since a process-wide recorder can be installed only once.
struct Observed {
    answer: Result<serde_json::Value, String>,
    stale: u64,
    unheard: u64,
    errors: Vec<String>,
}

fn ask(store: Option<Held>) -> Observed {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let calls = calls(store);
    let request = format!(r#"{{"account_id":"{}"}}"#, Uuid::new_v4());

    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    let answer = tracing::subscriber::with_default(subscriber, || {
        metrics::with_local_recorder(&recorder, || {
            calls.account_standing_impl(request.as_bytes())
        })
    });

    let rendered = handle.render();
    let count = |reason: &str| {
        rendered
            .lines()
            .find(|l| {
                l.starts_with(&format!(
                    "{}{{reason=\"{reason}\"}}",
                    data_service::FAIL_OPEN_COUNTER
                ))
            })
            .and_then(|l| l.rsplit(' ').next())
            .map_or(0, |v| v.parse().unwrap())
    };
    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    Observed {
        answer: answer.map(|b| serde_json::from_slice(&b).unwrap()),
        stale: count("stale"),
        unheard: count("unheard"),
        errors: logged
            .lines()
            .filter(|l| l.contains("ERROR") && l.contains("standing fail-open"))
            .map(str::to_owned)
            .collect(),
    }
}

fn held_store(row: HeldStanding) -> Option<Held> {
    Some(Held {
        row: Some(row),
        fail: false,
    })
}

/// The stale case end to end through the import a plugin reaches: a good
/// standing the sender has not confirmed within the bound is still handed to
/// the plugin, and the allow it implies is counted and logged at error.
#[test]
fn a_stale_good_standing_is_answered_and_surfaces_a_fail_open() {
    let o = ask(held_store(held(true, Utc::now() - Duration::days(8))));

    assert_eq!(o.answer.unwrap()["standing"]["in_good_standing"], true);
    assert_eq!(o.stale, 1, "the stale allow must move the counter");
    assert_eq!(o.errors.len(), 1, "and must be an error-level event");
}

#[test]
fn an_account_never_heard_of_is_null_and_surfaces_a_fail_open() {
    let o = ask(Some(Held {
        row: None,
        fail: false,
    }));

    assert!(o.answer.unwrap()["standing"].is_null());
    assert_eq!(o.unheard, 1);
    assert_eq!(o.errors.len(), 1);
}

/// The surfacing must not cry wolf: a confirmed good standing and a known-bad
/// one are not fail-open allows.
#[test]
fn a_fresh_good_standing_and_a_bad_one_surface_nothing() {
    let fresh = ask(held_store(held(true, Utc::now() - Duration::days(1))));
    assert_eq!(fresh.answer.unwrap()["standing"]["version"], 3);
    assert_eq!((fresh.stale, fresh.unheard, fresh.errors.len()), (0, 0, 0));

    let bad = ask(held_store(held(false, Utc::now() - Duration::days(30))));
    assert_eq!(bad.answer.unwrap()["standing"]["in_good_standing"], false);
    assert_eq!((bad.stale, bad.unheard, bad.errors.len()), (0, 0, 0));
}

/// An unreadable standing is an error, never "none": a plugin reads none as
/// permission to allow.
#[test]
fn a_store_failure_and_an_unpublished_store_are_errors_not_absence() {
    let down = ask(Some(Held {
        row: None,
        fail: true,
    }));
    assert!(down.answer.unwrap_err().contains("could not read"));
    assert_eq!(down.unheard, 0);

    let unpublished = ask(None);
    assert!(
        unpublished
            .answer
            .unwrap_err()
            .contains("does not report account standing")
    );
}

#[test]
fn an_account_id_that_is_not_an_account_id_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let calls = calls(held_store(held(true, Utc::now())));

    let err = calls.account_standing_impl(br#"{"account_id":"nope"}"#);
    assert!(err.unwrap_err().contains("is not an account id"));
}
