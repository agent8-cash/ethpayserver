//! Log level of the resubscribe decision for a stalled block stream.
//!
//! Its own binary for the same reason as `stalled_stream_still_pages.rs`:
//! `tracing`'s callsite-interest cache is process-wide, so a sibling test
//! that drives the same `resubscribe_if_stalled` call site with no subscriber
//! installed can cache it as uninteresting before this test's subscriber runs.

use std::sync::Arc;
use std::time::Duration;

use evm::monitor::{ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent, make_block};

const TEST_CHAIN_ID: u64 = 11155111;

fn test_chain_config() -> &'static evm::ChainConfig {
    evm::get_any_chain_config(TEST_CHAIN_ID).expect("Sepolia config exists")
}

fn fast_confirm_config() -> ChainMonitorConfig {
    ChainMonitorConfig {
        required_confirmations: 3,
        max_blocks_per_scan: 100,
        confirmation_check_interval_secs: 1,
        stall_timeout_secs: 1,
        loop_hang_timeout_secs: 5,
        ..ChainMonitorConfig::default()
    }
}

/// Shared, cloneable sink so a test can read back what a tracing subscriber wrote.
#[derive(Clone, Default)]
struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_block_stream_resubscribe_logs_warn_not_error() {
    // The resubscribe decision is logged before the backfill has said whether
    // the gap cost any data, so it must read as a warning, not an error.
    let logs = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    // current_thread runtime: the monitor task runs on this thread, so the
    // thread-local default covers it.
    let _guard = tracing::subscriber::set_default(subscriber);

    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        fast_confirm_config(),
    ));

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let _monitor_handle = tokio::spawn(async move { monitor_clone.start().await });

    let started = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;
    assert!(
        matches!(started, Ok(Ok(MonitorEvent::MonitorStarted { .. }))),
        "expected MonitorStarted"
    );

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(150)).await;

    let subscribes_before_stall = test_source.subscribe_count();
    test_source.set_block_number(130);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        test_source.subscribe_count() > subscribes_before_stall,
        "a silent stream must trigger a resubscribe"
    );

    let output = String::from_utf8(logs.0.lock().unwrap().clone()).expect("utf8 log output");
    let line = output
        .lines()
        .find(|l| l.contains("block stream is not delivering; resubscribing"))
        .unwrap_or_else(|| panic!("expected resubscribe log line, got: {output}"));
    assert!(line.contains("WARN"), "expected WARN level, got: {line}");
    assert!(
        !line.contains("ERROR"),
        "expected no ERROR level, got: {line}"
    );
}
