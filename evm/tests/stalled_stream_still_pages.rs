//! `evm::telemetry::sentry_event_filter`'s own tests prove it demotes a
//! hand-typed `alloy_transport_ws` target and still pages a hand-typed
//! `evm::monitor::chain::lifecycle` one, and `ws_pubsub_retry_escalation.rs`
//! proves the real `alloy_pubsub` retry loop never escalates on its own
//! against a flapping connection. Neither exercises `resubscribe_if_stalled`
//! itself: this drives a real stall through a real `ChainMonitor` and checks
//! what its own sustained-stall `error!` - emitted by the macro, not typed
//! into a test - is actually resolved to by the real filter. A single stall
//! that self-heals is only a `warn!`; a connection whose resubscribe does not
//! restore delivery is the one that must page. That closes the gap a doc
//! comment can only assert: a connection `alloy_transport_ws` never
//! recovers from still pages, even though every breadcrumb it logs along
//! the way does not.
//!
//! This lives in its own binary, not alongside `monitor_recovery.rs`'s other
//! stall tests, because `tracing`'s callsite-interest cache is process-wide:
//! a sibling test that drives the same `resubscribe_if_stalled` call site on
//! its own thread, with no subscriber installed, can permanently cache that
//! callsite as "nobody's listening" before this test's subscriber ever gets
//! a look at it, and a thread-local `set_default` here does not reliably
//! undo that once it's cached. A dedicated binary means this is the only
//! test that ever touches the call site, so there is nothing to race.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use evm::monitor::{ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent, make_block};
use tracing_subscriber::layer::SubscriberExt;

const TEST_CHAIN_ID: u64 = 11155111;

fn test_chain_config() -> &'static evm::ChainConfig {
    evm::get_any_chain_config(TEST_CHAIN_ID).expect("Sepolia config exists")
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(String, sentry_tracing::EventFilter)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() != tracing::Level::ERROR {
            return;
        }
        let filter = evm::telemetry::sentry_event_filter(tracing::Level::WARN)(event.metadata());
        self.0
            .lock()
            .expect("lock poisoned")
            .push((event.metadata().target().to_owned(), filter));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_sustained_stall_still_pages_through_the_real_sentry_filter() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    // Thread-local, not global: the runtime below is `current_thread`, so
    // this covers the spawned monitor task too, the same way
    // ws_pubsub_retry_escalation.rs covers alloy's background retry task.
    let _guard = tracing::subscriber::set_default(subscriber);

    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let config = ChainMonitorConfig {
        confirmation_check_interval_secs: 1,
        stall_timeout_secs: 1,
        ..ChainMonitorConfig::default()
    };
    let monitor = Arc::new(ChainMonitor::new(test_chain_config(), source, config));

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let _monitor_handle = tokio::spawn(async move { monitor_clone.start().await });

    let started = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;
    assert!(
        matches!(started, Ok(Ok(MonitorEvent::MonitorStarted { .. }))),
        "expected MonitorStarted"
    );

    test_source.push_block(make_block(300));
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Connected, but the stream has stopped delivering - the exact stall
    // `resubscribe_if_stalled` exists to notice.
    test_source.set_block_number(330);

    // Polls rather than sleeping a fixed budget: the confirmation-check tick
    // that drives `resubscribe_if_stalled` runs on its own clock, and a
    // fixed sleep would race it under load rather than actually prove
    // anything about what the filter resolves the event to.
    let lifecycle_event = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(found) = capture
                .0
                .lock()
                .expect("lock poisoned")
                .iter()
                .find(|(target, _)| target.starts_with("evm::monitor::chain::lifecycle"))
                .cloned()
            {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a sustained stall must log an error within a few stall-timeout cycles");

    assert!(
        lifecycle_event
            .1
            .contains(sentry_tracing::EventFilter::Event),
        "a sustained stall must still page even though alloy_transport_ws \
         targets are demoted, got {lifecycle_event:?}"
    );
}
