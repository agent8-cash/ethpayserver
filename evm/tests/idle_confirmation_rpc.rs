//! A quiet chain with nothing pending must cost nothing: no `get_block_number`
//! call from `check_confirmations`, and none from `resubscribe_if_stalled`'s
//! common (healthy, connected) path either. Before this, every chain made two
//! billed RPC calls on every confirmation-check tick forever, regardless of
//! invoice count - testnet burned through a provider's free tier idling.
//!
//! The risk in this fix is not "does it compile", it's "does it silently stop
//! confirming a payment or silently blind the stall detector". Every test
//! here drives the real `ChainMonitor::start()` event loop end to end, the
//! way `monitor_recovery.rs` does, rather than unit-testing a function in
//! isolation - the whole hazard is in how the skip interacts with the running
//! loop over several ticks.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use evm::monitor::{
    ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent, WatchedAddress, make_block,
    make_native_transfer,
};
use evm::{Address, B256, U256};

const TEST_CHAIN_ID: u64 = 11155111;

fn test_chain_config() -> &'static evm::ChainConfig {
    evm::get_any_chain_config(TEST_CHAIN_ID).expect("Sepolia config exists")
}

/// Short confirmation-check interval so several ticks elapse within a test's
/// patience; production defaults to 30s.
fn fast_confirm_config(required_confirmations: u64) -> ChainMonitorConfig {
    ChainMonitorConfig {
        required_confirmations,
        max_blocks_per_scan: 100,
        confirmation_check_interval_secs: 1,
        // Long relative to these tests' sleeps, except in
        // `stall_detection_still_works_with_nothing_watched_or_pending`,
        // which sets its own short value - nothing here should be mistaken
        // for a dead stream.
        stall_timeout_secs: 120,
        loop_hang_timeout_secs: 300,
        monitor_native: true,
        monitor_erc20: true,
        rescan_max_blocks: 10_000,
    }
}

async fn start_and_wait_ready(monitor: &Arc<ChainMonitor<MockBlockSource>>) {
    let mut events = monitor.subscribe();
    let running = Arc::clone(monitor);
    tokio::spawn(async move { running.start().await });
    let started = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert!(
        matches!(started, Ok(Ok(MonitorEvent::MonitorStarted { .. }))),
        "expected MonitorStarted"
    );
}

/// Scenario 1 + 6: zero watched, zero pending, source healthy and connected -
/// across several real confirmation-check ticks, neither call site that used
/// to call `get_block_number` unconditionally (`check_confirmations`,
/// `resubscribe_if_stalled`'s common path) should call it at all.
#[tokio::test]
async fn idle_chain_with_nothing_pending_makes_no_block_number_calls() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let probe = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        fast_confirm_config(3),
    ));
    start_and_wait_ready(&monitor).await;

    probe.reset_call_counts();

    // Several confirmation-check ticks (1s interval) elapse with nothing
    // watched and nothing pending.
    tokio::time::sleep(Duration::from_millis(3500)).await;

    assert_eq!(
        probe.call_count("get_block_number"),
        0,
        "an idle chain with nothing pending must not call get_block_number at all; \
         calls so far: {:?}",
        probe.call_counts()
    );
}

/// Scenario 2: the moment a payment is actually pending, the tick resumes
/// calling `get_block_number`, and confirmation still completes end to end -
/// not just that the call count moved, but that a real `PaymentConfirmed`
/// event is observed.
#[tokio::test]
async fn a_pending_payment_restores_the_tick_and_still_confirms() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let probe = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(1_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        fast_confirm_config(2),
    ));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut events = monitor.subscribe();
    start_and_wait_ready(&monitor).await;
    // `start_and_wait_ready` consumes its own `MonitorStarted` off a fresh
    // subscriber; this test needs to observe events too, so subscribe before
    // starting and drain that same event here.
    let started = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert!(matches!(
        started,
        Ok(Ok(MonitorEvent::MonitorStarted { .. }))
    ));

    probe.reset_call_counts();

    probe
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;
    probe.push_block(make_block(100));

    let detected = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("timeout waiting for PaymentDetected")
        .expect("channel error");
    assert!(
        matches!(detected, MonitorEvent::PaymentDetected(_)),
        "expected PaymentDetected, got {detected:?}"
    );

    // One confirmation at block 100; needs 2. Advance the head so the next
    // tick's get_block_number sees enough depth.
    probe.set_block_number(101);

    let confirmed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(MonitorEvent::PaymentConfirmed(c)) = events.recv().await {
                break c;
            }
        }
    })
    .await
    .expect("expected PaymentConfirmed once pending and the tick resumed");

    assert_eq!(confirmed.tx_hash, tx_hash);
    assert_eq!(confirmed.invoice_id, invoice_id);
    assert!(
        probe.call_count("get_block_number") >= 1,
        "the tick must have actually called get_block_number to notice the depth"
    );
}

/// Scenario 3: once the payment above confirms and leaves `pending`, calls
/// drop back to zero - catches a "sticky" implementation that keeps polling
/// forever once triggered once.
#[tokio::test]
async fn calls_drop_back_to_zero_once_pending_empties_again() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let probe = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(1_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        fast_confirm_config(1),
    ));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut events = monitor.subscribe();
    start_and_wait_ready(&monitor).await;
    let started = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert!(matches!(
        started,
        Ok(Ok(MonitorEvent::MonitorStarted { .. }))
    ));

    probe
        .add_native_transfer(
            200,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;
    probe.push_block(make_block(200));

    let detected = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("timeout waiting for PaymentDetected")
        .expect("channel error");
    assert!(matches!(detected, MonitorEvent::PaymentDetected(_)));

    // required_confirmations == 1, so block 200 itself is already enough -
    // the very next tick confirms it and `pending` empties.
    let confirmed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(MonitorEvent::PaymentConfirmed(c)) = events.recv().await {
                break c;
            }
        }
    })
    .await
    .expect("expected PaymentConfirmed");
    assert_eq!(confirmed.tx_hash, tx_hash);

    probe.reset_call_counts();
    tokio::time::sleep(Duration::from_millis(3500)).await;

    assert_eq!(
        probe.call_count("get_block_number"),
        0,
        "pending is empty again; the tick must go back to making no calls, not keep \
         polling because it once had something to confirm. Calls: {:?}",
        probe.call_counts()
    );
}

/// Scenario 4 (the specific guardrail): a payment can still be awaiting
/// confirmation after its address has been unwatched - the invoice's
/// grace-period unwatch can land before confirmation finishes. Gating the
/// skip on `watched` instead of `pending` would silently stop confirming it.
#[tokio::test]
async fn an_unwatched_but_still_pending_payment_still_confirms() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let probe = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(1_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        fast_confirm_config(2),
    ));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut events = monitor.subscribe();
    start_and_wait_ready(&monitor).await;
    let started = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert!(matches!(
        started,
        Ok(Ok(MonitorEvent::MonitorStarted { .. }))
    ));

    probe
        .add_native_transfer(
            300,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;
    probe.push_block(make_block(300));

    let detected = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("timeout waiting for PaymentDetected")
        .expect("channel error");
    assert!(matches!(detected, MonitorEvent::PaymentDetected(_)));

    // Unwatch before the second confirmation lands - the payment is now
    // pending-but-unwatched, the exact case the gate must still cover.
    let removed = monitor.unwatch(&payment_address, None).await;
    assert!(removed.is_some(), "precondition: the address was watched");
    assert!(
        monitor.watched_addresses().await.is_empty(),
        "precondition: nothing is watched anymore"
    );

    probe.reset_call_counts();
    probe.set_block_number(301);

    let confirmed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(MonitorEvent::PaymentConfirmed(c)) = events.recv().await {
                break c;
            }
        }
    })
    .await
    .expect(
        "a pending-but-unwatched payment must still confirm - gating the skip on \
         `watched` instead of `pending` would silently drop this",
    );
    assert_eq!(confirmed.tx_hash, tx_hash);
    assert_eq!(confirmed.invoice_id, invoice_id);
    assert!(
        probe.call_count("get_block_number") >= 1,
        "confirming it required the tick to actually call get_block_number"
    );
}

/// Scenario 5, the dangerous wrong version of this fix: nothing watched and
/// nothing pending must NOT also mean the stall watchdog stops working. A
/// dead stream with zero invoices today still has to be noticed the instant
/// an invoice is created a second later.
#[tokio::test]
async fn stall_detection_still_works_with_nothing_watched_or_pending() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let config = ChainMonitorConfig {
        stall_timeout_secs: 1,
        ..fast_confirm_config(3)
    };
    let monitor = Arc::new(ChainMonitor::new(test_chain_config(), source, config));

    start_and_wait_ready(&monitor).await;

    // Baseline: nothing watched, nothing pending, stream has delivered once.
    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(150)).await;

    let subscribes_before_stall = test_source.subscribe_count();

    // The stream goes silent (chain head still moves, exactly like a
    // half-open WebSocket that keeps the RPC reachable while the
    // subscription itself delivers nothing).
    test_source.set_block_number(130);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        test_source.subscribe_count() > subscribes_before_stall,
        "a stalled stream must still trigger a resubscribe even with nothing \
         watched and nothing pending - this is the one case none of the \
         existing recovery tests cover, and it's exactly what this fix must \
         not break"
    );
}
