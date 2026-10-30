//! Integration tests for the payment detection pipeline.
//!
//! Tests the full ChainMonitor flow: watch address, inject synthetic blocks,
//! verify PaymentDetected and PaymentConfirmed events are emitted correctly.

use std::sync::Arc;
use std::time::Duration;

use evm::monitor::{
    BACKFILL_MAX_BLOCKS, ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent,
    WatchedAddress, make_block, make_block_with_parent, make_erc20_transfer_log,
    make_native_transfer,
};
use evm::{Address, B256, U256};

use chrono::Utc;

/// Sepolia chain ID.
const TEST_CHAIN_ID: u64 = 11155111;

fn test_chain_config() -> &'static evm::ChainConfig {
    evm::get_any_chain_config(TEST_CHAIN_ID).expect("Sepolia config exists")
}

fn test_monitor_config(required_confirmations: u64) -> ChainMonitorConfig {
    ChainMonitorConfig {
        required_confirmations,
        max_blocks_per_scan: 100,
        // Short interval so confirmation checks happen quickly in tests
        confirmation_check_interval_secs: 1,
        stall_timeout_secs: 120,
        loop_hang_timeout_secs: 300,
        monitor_native: true,
        monitor_erc20: true,
    }
}

// ============================================================================
// Native ETH payment: detection + confirmation
// ============================================================================

#[tokio::test]
#[allow(clippy::too_many_lines)] // end-to-end payment flow test with multi-step setup + assertions
async fn test_native_payment_detection_and_confirmation() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone(); // shared handle for injection

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(50_000_000_000_000_000u64); // 0.05 ETH
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    // Watch the address for native ETH
    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();

    // Start monitor
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });

    // Wait for MonitorStarted
    let started = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;
    assert!(
        matches!(started, Ok(Ok(MonitorEvent::MonitorStarted { .. }))),
        "expected MonitorStarted"
    );

    // Simulate payment: set new balance and add the transfer
    test_source
        .set_balance(payment_address, payment_amount)
        .await;
    test_source
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;

    // Push block 100
    test_source.push_block(make_block(100));

    // Expect PaymentDetected
    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("channel error");

    match event {
        MonitorEvent::PaymentDetected(p) => {
            assert_eq!(p.chain_id, TEST_CHAIN_ID);
            assert_eq!(p.invoice_id, invoice_id);
            assert_eq!(p.payment_address, payment_address);
            assert_eq!(p.amount, payment_amount);
            assert_eq!(p.tx_hash, tx_hash);
            assert_eq!(p.block_number, 100);
            assert!(p.is_native);
            assert!(p.token_address.is_none());
            assert_eq!(p.from_address, sender);
            assert_eq!(p.confirmations, 1);
            assert_eq!(p.required_confirmations, 3);
        }
        other => panic!("expected PaymentDetected, got {:?}", other),
    }

    // Advance blocks to trigger confirmation (need 3 confirmations: blocks 100, 101, 102)
    // Block 101 (2 confirmations)
    test_source.push_block(make_block(101));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Block 102 (3 confirmations) — should trigger PaymentConfirmed
    test_source.push_block(make_block(102));

    // Wait for confirmation check to fire (up to 2 seconds)
    let mut confirmed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await {
            Ok(Ok(MonitorEvent::PaymentConfirmed(c))) => {
                assert_eq!(c.chain_id, TEST_CHAIN_ID);
                assert_eq!(c.invoice_id, invoice_id);
                assert_eq!(c.tx_hash, tx_hash);
                assert_eq!(c.amount, payment_amount);
                assert!(c.confirmations >= 3);
                confirmed = true;
                break;
            }
            Ok(Ok(_)) => continue, // skip other events
            _ => continue,
        }
    }
    assert!(confirmed, "expected PaymentConfirmed within timeout");

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

// ============================================================================
// ERC20 token payment detection
// ============================================================================

#[tokio::test]
async fn test_erc20_payment_detection() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let token_contract = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(100_000_000u64); // 100 USDT (6 decimals)
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    // Watch address for ERC20 token (token_contract = Some)
    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: Some(token_contract),
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();

    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });

    // Wait for MonitorStarted
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    // Inject ERC20 Transfer log for block 200
    test_source
        .add_log(
            200,
            make_erc20_transfer_log(
                token_contract,
                sender,
                payment_address,
                payment_amount,
                200,
                tx_hash,
                0,
            ),
        )
        .await;

    // Push block 200
    test_source.push_block(make_block(200));

    // Expect PaymentDetected
    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("channel error");

    match event {
        MonitorEvent::PaymentDetected(p) => {
            assert_eq!(p.chain_id, TEST_CHAIN_ID);
            assert_eq!(p.invoice_id, invoice_id);
            assert_eq!(p.payment_address, payment_address);
            assert_eq!(p.amount, payment_amount);
            assert_eq!(p.tx_hash, tx_hash);
            assert!(!p.is_native);
            assert_eq!(p.token_address, Some(token_contract));
            assert_eq!(p.from_address, sender);
            assert_eq!(p.log_index, Some(0));
        }
        other => panic!("expected PaymentDetected, got {:?}", other),
    }

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

// ============================================================================
// Underpayment: partial payment keeps pending, second completes
// ============================================================================

#[tokio::test]
async fn test_underpayment_two_transactions() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let half_amount = U256::from(25_000_000_000_000_000u64); // 0.025 ETH
    let tx_hash_1 = B256::random();
    let tx_hash_2 = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(1), // 1 confirmation for simplicity
    ));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(half_amount * U256::from(2)),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });

    // Wait for start
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    // First partial payment
    test_source.set_balance(payment_address, half_amount).await;
    test_source
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, half_amount, tx_hash_1),
        )
        .await;
    test_source.push_block(make_block(100));

    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout")
        .expect("channel error");
    match &event {
        MonitorEvent::PaymentDetected(p) => {
            assert_eq!(p.amount, half_amount);
            assert_eq!(p.tx_hash, tx_hash_1);
        }
        other => panic!("expected first PaymentDetected, got {:?}", other),
    }

    // Second partial payment (completes the full amount)
    let full_amount = half_amount * U256::from(2);
    test_source.set_balance(payment_address, full_amount).await;
    test_source
        .add_native_transfer(
            101,
            make_native_transfer(sender, payment_address, half_amount, tx_hash_2),
        )
        .await;
    test_source.push_block(make_block(101));

    // Should get second PaymentDetected
    let mut got_second = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await {
            Ok(Ok(MonitorEvent::PaymentDetected(p))) if p.tx_hash == tx_hash_2 => {
                assert_eq!(p.amount, half_amount);
                got_second = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => continue,
        }
    }
    assert!(got_second, "expected second PaymentDetected");

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

// ============================================================================
// No watched addresses = no events
// ============================================================================

#[tokio::test]
async fn test_no_watched_addresses_no_events() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });

    // Wait for start
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    // Push blocks with no watches
    test_source.push_block(make_block(100));
    test_source.push_block(make_block(101));

    // Should not receive any payment events
    let result = tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await;
    if let Ok(Ok(MonitorEvent::PaymentDetected(_))) = result {
        panic!("should not detect payment with no watches")
    }

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

// ============================================================================
// Reorg detection and re-validation
// ============================================================================

/// Wait for the next `ReorgDetected` event, skipping anything else.
async fn wait_for_reorg(
    event_rx: &mut tokio::sync::broadcast::Receiver<MonitorEvent>,
) -> evm::monitor::events::ReorgDetected {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await {
            Ok(Ok(MonitorEvent::ReorgDetected(r))) => return r,
            Ok(Ok(_)) => continue,
            _ => continue,
        }
    }
    panic!("timed out waiting for ReorgDetected");
}

/// A fork arriving more than one block ahead of the last processed block used
/// to go unnoticed entirely: `process_block` only ever compared `parent_hash`
/// when the new block was the immediate successor. Here block 103 arrives
/// right after block 100, skipping 101 and 102, so that direct comparison
/// never runs — detection must fall back to asking the chain whether block
/// 100 is still canonical.
#[tokio::test]
async fn test_reorg_detected_across_a_block_gap() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Block 100 was reorged out from under us, but the next notification
    // skips straight to block 103. 101 and 102 are registered directly
    // (not pushed as notifications, which would advance `last_block` past
    // the gap this test means to exercise) so the reorg re-validation scan
    // - which now reads every block in `[fork_block, new_block]` directly
    // when nothing is watched - has something to find at each height.
    test_source.set_block_hash(100, B256::random());
    test_source.set_block_hash(101, B256::random());
    test_source.set_block_hash(102, B256::random());
    test_source.push_block(make_block(103));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(reorg.fork_block, 100);

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// A block arriving at or behind an already-processed height (not the
/// documented gap-*forward* case above) takes the same continuity-check
/// branch, but the only fact it can establish is that the chain's current
/// hash at `last_num` no longer matches what was recorded — it has no record
/// of any hash below `last_num` to compare against. So `fork_block` is a
/// best-effort guess (the lower of `last_num` and the incoming block's own
/// number), not a verified bound: it is only correct if the true fork point
/// happens to be at or above the incoming block's number. A fork deeper than
/// that is guessed too shallow, under-including candidates — documented and
/// accepted as residual scope (see the comment above this branch in
/// `process_block`), since closing it needs retained per-block history this
/// monitor does not keep. This test pins that guess so a future change to it
/// is deliberate, not accidental.
#[tokio::test]
async fn test_reorg_backward_jump_guesses_fork_block_from_incoming_block_number() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The chain's canonical hash at 100 no longer matches, and rather than a
    // forward gap, the next notification is for an *earlier* height (98) -
    // e.g. a provider re-delivering after reconnecting mid-reorg.
    test_source.set_block_hash(100, B256::random());
    test_source.push_block(make_block(98));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(
        reorg.fork_block, 98,
        "guess is min(last_num, incoming) = 98"
    );

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// If the RPC call backing the gap-continuity check itself fails, that must
/// not be treated the same as "checked, no reorg": falling through to `None`
/// would advance `last_block` past block 100 as if continuity were confirmed,
/// permanently losing the one chance to catch the fork. The failure must
/// instead leave `last_block` at 100 so the same gap is re-checked on the
/// next block.
#[tokio::test]
async fn test_reorg_gap_check_rpc_failure_does_not_lose_the_reorg() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Block 100 was reorged out, and the next notification skips to 103 (a
    // gap), so continuity can only be checked via get_block_hash. Make that
    // RPC call fail.
    test_source.set_block_hash(100, B256::random());
    test_source.set_get_block_hash_error(Some("rpc unavailable"));
    test_source.push_block(make_block(103));

    // Nothing must be reported while the check itself couldn't run - not a
    // reorg, and not silence that looks identical to "verified clean".
    let outcome = tokio::time::timeout(Duration::from_millis(300), event_rx.recv()).await;
    assert!(
        !matches!(outcome, Ok(Ok(MonitorEvent::ReorgDetected(_)))),
        "must not report a reorg when the gap check itself failed to run: {outcome:?}"
    );

    // Once the RPC recovers, the fork must still be found against the
    // original last_num (100) - proving it was never advanced past the
    // failed check. 101-103 are registered directly (not pushed, which
    // would advance `last_block` past the gap) so the reorg re-validation
    // scan - which now reads every block in range directly when nothing is
    // watched - has something to find at each height.
    test_source.set_get_block_hash_error(None);
    test_source.set_block_hash(101, B256::random());
    test_source.set_block_hash(102, B256::random());
    test_source.set_block_hash(103, B256::random());
    test_source.push_block(make_block(104));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(reorg.fork_block, 100);

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// A transaction that survives a reorg by landing in a different block must
/// be reported as survived, not treated as gone. Retracting it anyway is the
/// opposite error: it un-pays an invoice that a customer genuinely settled.
/// This is the case a naive "everything at or above the fork block is gone"
/// fix gets wrong.
#[tokio::test]
async fn test_reorg_reports_a_relocated_transaction_as_survived() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(50_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
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

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    // The payment is first detected at block 100.
    test_source
        .set_balance(payment_address, payment_amount)
        .await;
    test_source
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;
    test_source.push_block(make_block(100));

    let detected = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for PaymentDetected")
        .expect("channel error");
    assert!(matches!(detected, MonitorEvent::PaymentDetected(_)));

    // A reorg replaces block 100, but the very same transaction lands again
    // at block 101 on the new canonical chain rather than disappearing.
    test_source
        .add_native_transfer(
            101,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;
    test_source.push_block(make_block_with_parent(101, B256::random(), B256::random()));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(reorg.fork_block, 100);
    assert!(
        reorg.survived_tx_hashes.contains(&tx_hash),
        "a transaction relocated to a different block must be reported as survived, not gone"
    );

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// The same relocation guarantee must hold for ERC20 payments, not just
/// native transfers: `find_survived_tx_hashes` re-validates them through a
/// completely different code path (`get_logs` with a Transfer-event filter
/// rather than `find_native_transfers_to`), which none of the other reorg
/// tests exercise.
#[tokio::test]
async fn test_reorg_reports_a_relocated_erc20_transfer_as_survived() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let token_contract = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(100_000_000u64); // 100 USDT (6 decimals)
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: Some(token_contract),
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    // The payment is first detected at block 100.
    test_source
        .add_log(
            100,
            make_erc20_transfer_log(
                token_contract,
                sender,
                payment_address,
                payment_amount,
                100,
                tx_hash,
                0,
            ),
        )
        .await;
    test_source.push_block(make_block(100));

    let detected = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for PaymentDetected")
        .expect("channel error");
    assert!(matches!(detected, MonitorEvent::PaymentDetected(_)));

    // A reorg replaces block 100, but the very same transaction lands again
    // at block 101 on the new canonical chain rather than disappearing.
    test_source
        .add_log(
            101,
            make_erc20_transfer_log(
                token_contract,
                sender,
                payment_address,
                payment_amount,
                101,
                tx_hash,
                0,
            ),
        )
        .await;
    test_source.push_block(make_block_with_parent(101, B256::random(), B256::random()));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(reorg.fork_block, 100);
    assert!(
        reorg.survived_tx_hashes.contains(&tx_hash),
        "a relocated ERC20 transfer must be reported as survived, not gone"
    );

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// A reorg must still be verifiable - and a transaction it genuinely dropped
/// must still come back absent from `survived_tx_hashes` - when nothing is
/// watched. Nothing watched is the ordinary state of a quiet server once
/// every invoice has settled and passed its grace period, not a corner case:
/// before this, an empty watch set made the whole reorg unverifiable, so the
/// server's reorg handler retracted nothing and a payment the chain really
/// dropped stayed marked valid forever.
///
/// Goes red without the empty-watch-set fallback in `find_survived_tx_hashes`:
/// `survivors_verifiable` comes back `false` and the dropped transaction's
/// absence is never reported at all.
#[tokio::test]
async fn test_reorg_with_nothing_watched_is_still_verifiable() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    // Deliberately nothing watched.
    let (mut event_rx, monitor_handle) = start_and_wait(&monitor).await;

    let dropped_tx = B256::random();
    test_source.set_block_transactions(100, vec![dropped_tx]);
    test_source.push_block(make_block(100));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The reorg replaces block 100 with one that does not carry the
    // transaction, and block 101 on the new chain has a different parent.
    test_source.set_block_transactions(100, vec![]);
    test_source.push_block(make_block(101));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(reorg.fork_block, 100);
    assert!(
        reorg.survivors_verifiable,
        "an empty watch set must not make the reorg unverifiable - the chain's own \
         block contents can still be checked directly"
    );
    assert!(
        !reorg.survived_tx_hashes.contains(&dropped_tx),
        "a transaction genuinely dropped by the reorg must not be reported as survived, \
         even with nothing watched"
    );

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// A reorg window wider than `max_blocks_per_scan` must still find a
/// relocated transaction anywhere in `[fork_block, new_head]`, not just in
/// the tail nearest the new head. Clamping the re-scan window to that knob —
/// the same one ordinary block processing uses to bound history scans —
/// silently drops survivors below the clamp and retracts them anyway: the
/// opposite error, on a wider reorg than the single-block case above
/// exercises.
#[tokio::test]
async fn test_reorg_wider_than_scan_cap_still_finds_a_relocated_transaction() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(50_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let mut config = test_monitor_config(3);
    config.max_blocks_per_scan = 2;

    let monitor = Arc::new(ChainMonitor::new(test_chain_config(), source, config));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(payment_amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The transaction relocates to block 105 — far below the last
    // `max_blocks_per_scan` (2) blocks of the eventual [100, 130] window.
    test_source
        .add_native_transfer(
            105,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;

    // Block 100 is reorged out, and the chain jumps straight to block 130 —
    // a fork window far wider than `max_blocks_per_scan`.
    test_source.set_block_hash(100, B256::random());
    test_source.push_block(make_block(130));

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(reorg.fork_block, 100);
    assert!(
        reorg.survived_tx_hashes.contains(&tx_hash),
        "a transaction relocated below the scan cap's tail must still be found, not skipped"
    );

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// When re-validation can't reach the chain (RPC error), the monitor must not
/// report the reorg at all — reporting it with an empty `survived_tx_hashes`
/// would make the caller retract every candidate payment, on a mere hiccup.
/// This is the same "opposite error" the naive fix made, just triggered by a
/// transient failure instead of a naive implementation.
///
/// It must also retry: once the chain is reachable again, the very next
/// block re-evaluates the same reorg and reports it correctly.
#[tokio::test]
async fn test_reorg_revalidation_failure_reports_nothing_and_retries() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let payment_amount = U256::from(50_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
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

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    // Block 100 is processed normally: this must succeed even though the
    // fault we inject below targets the very same RPC call, so the fault is
    // armed only after detection has already used it once.
    test_source
        .set_balance(payment_address, payment_amount)
        .await;
    test_source
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, payment_amount, tx_hash),
        )
        .await;
    let block_100 = make_block(100);
    let block_100_hash = block_100.hash;
    test_source.push_block(block_100);

    let detected = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for PaymentDetected")
        .expect("channel error");
    assert!(matches!(detected, MonitorEvent::PaymentDetected(_)));

    // Now a reorg arrives, but the chain is unreachable for re-validation.
    test_source.set_find_native_transfers_error(Some("mock RPC timeout"));
    let forking_block = make_block_with_parent(101, B256::random(), B256::random());
    assert_ne!(forking_block.parent_hash, block_100_hash);
    test_source.push_block(forking_block.clone());

    // A MonitorError must appear instead of a ReorgDetected: reporting the
    // reorg with nothing marked as survived would retract a payment we never
    // actually re-validated.
    let after_failure = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for MonitorError")
        .expect("channel error");
    assert!(
        matches!(after_failure, MonitorEvent::MonitorError { .. }),
        "expected MonitorError after a failed re-validation, got {:?}",
        after_failure
    );

    // The chain is reachable again. Re-pushing the same block must retry and
    // now succeed, because the failed attempt never advanced past block 100.
    test_source.set_find_native_transfers_error(None);
    test_source.push_block(forking_block);

    let reorg = wait_for_reorg(&mut event_rx).await;
    assert_eq!(
        reorg.fork_block, 100,
        "the retry must still find the same fork point"
    );

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
}

/// Subscribe, start the monitor, and wait for it to report itself up.
///
/// No test here is testing this, so folding it away keeps what a test actually
/// asserts visible rather than buried under four lines of identical setup. The
/// eleven older tests in this file still each carry their own copy; worth
/// collapsing onto this when one of them is next touched, rather than churning
/// them all in a change about payment detection.
async fn start_and_wait(
    monitor: &Arc<ChainMonitor<MockBlockSource>>,
) -> (
    tokio::sync::broadcast::Receiver<MonitorEvent>,
    tokio::task::JoinHandle<evm::EvmResult<()>>,
) {
    let mut event_rx = monitor.subscribe();
    let running = Arc::clone(monitor);
    let handle = tokio::spawn(async move { running.start().await });

    let started = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;
    assert!(
        matches!(started, Ok(Ok(MonitorEvent::MonitorStarted { .. }))),
        "monitor did not start: {started:?}"
    );

    (event_rx, handle)
}

/// Two payments to one address, the second smaller than the first, are both
/// detected.
///
/// This pins a failure mode shut rather than guarding live code. Detection
/// used to work on balance *increases* against a stored `last_known_balance`,
/// which made "smaller than something seen earlier" a meaningful and
/// dangerous category: a merchant sweeping a payment address dropped its
/// balance, and if the stored value stayed at the pre-sweep high-water mark,
/// a later genuine payment below that mark never read as an increase. The
/// customer paid and nothing credited it, silently.
///
/// Detection now reads the block's transfers, so amounts are not compared
/// against remembered state at all and the sweep here is invisible to the
/// monitor. The test is kept because it is cheap and because it is the
/// assertion that goes red if detection is ever rebuilt on remembered
/// balances - every other test in this file sends a single payment, or
/// payments that only grow.
#[tokio::test]
async fn a_payment_after_a_sweep_is_still_detected() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let invoice_id = uuid::Uuid::new_v4();

    // The second payment is deliberately *smaller* than the first. If
    // `last_known_balance` is left at the pre-sweep figure, this cannot look
    // like an increase, which is the whole point of the test.
    let first_payment = U256::from(50_000_000_000_000_000u64); // 0.05 ETH
    let second_payment = U256::from(10_000_000_000_000_000u64); // 0.01 ETH

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));

    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id,
            expected_amount: Some(first_payment),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let (mut event_rx, monitor_handle) = start_and_wait(&monitor).await;

    // 1. The customer pays.
    let first_tx = B256::random();
    test_source
        .set_balance(payment_address, first_payment)
        .await;
    test_source
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, first_payment, first_tx),
        )
        .await;
    test_source.push_block(make_block(100));

    let detected = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for the first payment")
        .expect("channel error");
    match detected {
        MonitorEvent::PaymentDetected(p) => assert_eq!(p.amount, first_payment),
        other => panic!("expected the first PaymentDetected, got {other:?}"),
    }

    // 2. The merchant sweeps the address. No transfer *to* it, so nothing
    //    should be detected for this block.
    test_source.push_block(make_block(101));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 3. A second, smaller payment arrives.
    let second_tx = B256::random();
    test_source
        .add_native_transfer(
            102,
            make_native_transfer(sender, payment_address, second_payment, second_tx),
        )
        .await;
    test_source.push_block(make_block(102));

    let mut saw_second = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), event_rx.recv()).await {
            Ok(Ok(MonitorEvent::PaymentDetected(p))) if p.tx_hash == second_tx => {
                assert_eq!(p.amount, second_payment);
                assert_eq!(p.payment_address, payment_address);
                saw_second = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }

    monitor_handle.abort();
    assert!(
        saw_second,
        "the second payment was never detected. A payment smaller than one seen \
         earlier must still be credited; if this fails, detection has been rebuilt on \
         remembered balances and a swept address silently stops crediting."
    );
}

/// A block the monitor could not read is reported, not treated as empty.
///
/// Native detection reads the block once and matches its transfers, so that
/// single call is all that stands between a payment and never being credited.
/// When it fails — the HTTP endpoint has not imported a block the
/// subscription already announced, which happens whenever the two are not the
/// same node — the monitor must say so.
///
/// The failure this pins shut is silence. `find_native_transfers_to`
/// returning `Ok(vec![])` for a block it could not fetch is indistinguishable
/// from a block containing no payments, so the loop would advance past it with
/// no error anywhere.
///
/// Note what this does **not** claim: the payment is not recovered. Nothing
/// re-reads a block the monitor has moved past — gaps are consulted only for
/// reorg detection and no catch-up exists — so the payment in that block is
/// lost either way. What changes is that it is lost *loudly*, with a
/// `MonitorError` naming the block, instead of looking like an ordinary empty
/// block forever. Backfilling a missed block is a separate gap and is tracked
/// on its own.
#[tokio::test]
async fn a_block_that_could_not_be_read_is_reported_rather_than_treated_as_empty() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let amount = U256::from(50_000_000_000_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));
    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id: uuid::Uuid::new_v4(),
            expected_amount: Some(amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let (mut event_rx, handle) = start_and_wait(&monitor).await;

    // The payment is in block 100, but reading that block fails.
    test_source
        .add_native_transfer(
            100,
            make_native_transfer(sender, payment_address, amount, tx_hash),
        )
        .await;
    test_source.set_find_native_transfers_error(Some("block not imported yet"));
    test_source.push_block(make_block(100));

    let mut reported = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(400), event_rx.recv()).await {
            Ok(Ok(MonitorEvent::MonitorError { error, .. })) => {
                assert!(
                    error.contains("block not imported yet"),
                    "the reported error should name what actually failed, got {error:?}"
                );
                reported = true;
                break;
            }
            Ok(Ok(MonitorEvent::PaymentDetected(_))) => {
                panic!("a payment cannot be detected from a block that could not be read")
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }

    handle.abort();
    assert!(
        reported,
        "a block read that failed was silently treated as a block with no payments. \
         Nothing re-reads it, so the payment inside it is gone with no trace."
    );
}

// ============================================================================
// Blocks skipped by a stream that went down and resubscribed
// ============================================================================

/// Watch one address, process block 100, then deliver block 103 with a
/// payment sitting in block 101 or 102. With `stalled` the stream never
/// delivered 101/102 (a resubscribe starts from the new head); without it they
/// arrive normally, which is the control: that the payment is found there
/// proves the fixture pays into a watched address, so a pass under a stall
/// means the gap was read and not that anything is reported regardless.
///
/// Returns the payments detected as `(block_number, tx_hash)`.
async fn payments_detected_across_a_stall(stalled: bool, erc20: bool) -> Vec<(u64, B256)> {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let token_contract = Address::random();
    let sender = Address::random();
    let amount = U256::from(1_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));
    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id: uuid::Uuid::new_v4(),
            expected_amount: Some(amount),
            token_contract: erc20.then_some(token_contract),
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The payment lands in block 101, inside what the stream skipped.
    if erc20 {
        test_source
            .add_log(
                101,
                make_erc20_transfer_log(
                    token_contract,
                    sender,
                    payment_address,
                    amount,
                    101,
                    tx_hash,
                    0,
                ),
            )
            .await;
    } else {
        test_source
            .add_native_transfer(
                101,
                make_native_transfer(sender, payment_address, amount, tx_hash),
            )
            .await;
    }

    if stalled {
        // The chain has them; the subscription never delivered them.
        test_source.set_block_hash(101, B256::random());
        test_source.set_block_hash(102, B256::random());
    } else {
        test_source.push_block(make_block(101));
        test_source.push_block(make_block(102));
    }
    test_source.push_block(make_block(103));

    let mut detected = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(MonitorEvent::PaymentDetected(p))) =
            tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await
        {
            detected.push((p.block_number, p.tx_hash));
        }
    }

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;
    detected
}

/// Nothing backfilled the blocks a resubscribe skipped: the cursor moved to
/// the new head and a payment in the gap was never credited.
#[tokio::test]
async fn test_native_payment_in_a_stalled_gap_is_detected() {
    let control = payments_detected_across_a_stall(false, false).await;
    assert!(
        control.len() == 1 && control[0].0 == 101,
        "control (no gap) must detect the payment exactly once, got {control:?}"
    );

    let detected = payments_detected_across_a_stall(true, false).await;
    assert_eq!(
        detected.len(),
        1,
        "a native payment in blocks the stream skipped must still be detected, got {detected:?}"
    );
    assert_eq!(detected[0].0, 101, "credited against the block it was in");
}

#[tokio::test]
async fn test_erc20_payment_in_a_stalled_gap_is_detected() {
    let control = payments_detected_across_a_stall(false, true).await;
    assert!(
        control.len() == 1 && control[0].0 == 101,
        "control (no gap) must detect the payment exactly once, got {control:?}"
    );

    let detected = payments_detected_across_a_stall(true, true).await;
    assert_eq!(
        detected.len(),
        1,
        "an ERC20 payment in blocks the stream skipped must still be detected, got {detected:?}"
    );
    assert_eq!(detected[0].0, 101, "credited against the block it was in");
}

/// A backfill that fails part-way is retried from the same height on the next
/// block. Payments the failed attempt had already read must not be announced
/// again by the retry: block 102 cannot be named, so the scan fails after
/// reading 101, and it fails again on the next block before finally succeeding.
#[tokio::test]
async fn test_failed_backfill_retry_reports_a_payment_once() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let amount = U256::from(1_000_000u64);
    let tx_hash = B256::random();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));
    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id: uuid::Uuid::new_v4(),
            expected_amount: Some(amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    test_source
        .add_native_transfer(
            101,
            make_native_transfer(sender, payment_address, amount, tx_hash),
        )
        .await;
    // 101 is readable; 102 is not, so every backfill attempt fails after 101.
    test_source.set_block_hash(101, B256::random());
    test_source.push_block(make_block(103));
    tokio::time::sleep(Duration::from_millis(100)).await;
    test_source.push_block(make_block(104));
    tokio::time::sleep(Duration::from_millis(100)).await;

    test_source.set_block_hash(102, B256::random());
    test_source.push_block(make_block(105));

    let mut detected = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(MonitorEvent::PaymentDetected(p))) =
            tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await
        {
            detected.push((p.block_number, p.tx_hash));
        }
    }

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;

    assert_eq!(
        detected,
        vec![(101, tx_hash)],
        "a payment read by a backfill attempt that then failed must be reported once, \
         when the range finally succeeds"
    );
}

/// A stall longer than one backfill chunk: the payment sits beyond the first
/// chunk, each arrival scans at most one chunk, and the payment is reported
/// once when the catch-up reaches it.
#[tokio::test]
async fn test_backfill_of_a_long_stall_is_bounded_and_still_finds_the_payment() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let payment_address = Address::random();
    let sender = Address::random();
    let amount = U256::from(1_000_000u64);
    let tx_hash = B256::random();
    let cap = BACKFILL_MAX_BLOCKS;
    // Gap of three chunks; the payment is in the second.
    let head = 100 + 3 * cap + 1;
    let payment_block = 100 + cap + 5;

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(3),
    ));
    monitor
        .watch(WatchedAddress {
            address: payment_address,
            invoice_id: uuid::Uuid::new_v4(),
            expected_amount: Some(amount),
            token_contract: None,
            created_at: Utc::now(),
        })
        .await;

    let mut event_rx = monitor.subscribe();
    let monitor_clone = monitor.clone();
    let monitor_handle = tokio::spawn(async move { monitor_clone.start().await });
    let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;

    test_source.push_block(make_block(100));
    tokio::time::sleep(Duration::from_millis(100)).await;

    test_source
        .add_native_transfer(
            payment_block,
            make_native_transfer(sender, payment_address, amount, tx_hash),
        )
        .await;
    for n in 101..head {
        test_source.set_block_hash(n, B256::random());
    }
    test_source.reset_call_counts();
    test_source.push_block(make_block(head));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // One arrival read one chunk and nothing beyond it, and the payment in a
    // later chunk is not reported yet.
    let scanned = test_source.call_count("find_native_transfers_to");
    assert!(
        scanned <= cap,
        "one block must scan at most {cap} blocks, scanned {scanned}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .is_err(),
        "the payment is beyond the first chunk and cannot be known yet"
    );

    // Following blocks continue from where the cursor stopped.
    for n in head + 1..=head + 4 {
        test_source.push_block(make_block(n));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut detected = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(MonitorEvent::PaymentDetected(p))) =
            tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await
        {
            detected.push((p.block_number, p.tx_hash));
        }
    }

    monitor.stop().await.unwrap();
    let _ = monitor_handle.await;

    assert_eq!(
        detected,
        vec![(payment_block, tx_hash)],
        "the payment must be detected exactly once after the catch-up reaches it"
    );
}
