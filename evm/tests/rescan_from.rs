//! `RescanFrom`: re-reading a block window for an explicit watch set.
//!
//! Recovery from an event-outbox lineage break needs a way to re-read chain
//! history for addresses the monitor's in-memory watch list may no longer
//! have (because the watch has since expired). These tests exercise that
//! path directly against `ChainMonitor::rescan_from`, without running the
//! live block-stream loop at all - the live loop is unchanged by this
//! feature, and the existing `payment_flow.rs`/`monitor_recovery.rs` suites
//! already cover it.

use std::sync::Arc;
use std::time::Duration;

use evm::monitor::{
    ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent, RescanWatch,
    make_erc20_transfer_log, make_native_transfer,
};
use evm::{Address, B256, U256};

/// Sepolia chain ID, used as a stand-in test chain elsewhere in this crate.
const TEST_CHAIN_ID: u64 = 11155111;

fn test_chain_config() -> &'static evm::ChainConfig {
    evm::get_any_chain_config(TEST_CHAIN_ID).expect("Sepolia config exists")
}

fn test_monitor_config(required_confirmations: u64, rescan_max_blocks: u64) -> ChainMonitorConfig {
    ChainMonitorConfig {
        required_confirmations,
        max_blocks_per_scan: 100,
        confirmation_check_interval_secs: 1,
        stall_timeout_secs: 120,
        loop_hang_timeout_secs: 300,
        monitor_native: true,
        monitor_erc20: true,
        rescan_max_blocks,
    }
}

/// Give the mock source a canonical hash for every block in `from..=to`, so
/// `rescan_from`'s per-block native scan (which asks for each block's hash
/// to label what it finds) has something to find. Real nodes always answer
/// this for a block that exists; the mock has to be told.
fn seed_block_hashes(source: &MockBlockSource, from: u64, to: u64) {
    for number in from..=to {
        source.set_block_hash(number, B256::random());
    }
}

async fn recv(rx: &mut tokio::sync::broadcast::Receiver<MonitorEvent>) -> Option<MonitorEvent> {
    tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .ok()
        .and_then(Result::ok)
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end scenario: native + ERC20 + an ignored address
async fn rescan_from_finds_native_and_erc20_in_window_and_ignores_unlisted_address() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let native_address = Address::random();
    let native_invoice = uuid::Uuid::new_v4();
    let erc20_address = Address::random();
    let erc20_invoice = uuid::Uuid::new_v4();
    let token = Address::random();
    let unlisted_address = Address::random();

    let native_amount = U256::from(50_000_000_000_000_000u64);
    let erc20_amount = U256::from(1_000_000u64);
    let native_tx = B256::random();
    let erc20_tx = B256::random();

    const FROM: u64 = 50;
    const TIP: u64 = 60;

    // A transfer to an address this rescan is not told about must be found
    // by the scan (it shares a block with the native transfer that must be
    // found) and then dropped, not reported.
    test_source
        .add_native_transfer(
            FROM,
            make_native_transfer(Address::random(), native_address, native_amount, native_tx),
        )
        .await;
    test_source
        .add_native_transfer(
            FROM,
            make_native_transfer(
                Address::random(),
                unlisted_address,
                U256::from(1),
                B256::random(),
            ),
        )
        .await;
    test_source
        .add_log(
            TIP,
            make_erc20_transfer_log(
                token,
                Address::random(),
                erc20_address,
                erc20_amount,
                TIP,
                erc20_tx,
                0,
            ),
        )
        .await;

    seed_block_hashes(&test_source, FROM, TIP);
    test_source.set_block_number(TIP);

    // Confirmations high enough that nothing confirms during this rescan;
    // the ordering test below covers the detect-then-confirm case.
    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(100, 10_000),
    ));
    let mut events = monitor.subscribe();

    monitor
        .rescan_from(
            FROM,
            vec![
                RescanWatch {
                    address: native_address,
                    invoice_id: native_invoice,
                    token_contract: None,
                },
                RescanWatch {
                    address: erc20_address,
                    invoice_id: erc20_invoice,
                    token_contract: Some(token),
                },
            ],
        )
        .await
        .expect("rescan within the cap must succeed");

    let mut found_native = false;
    let mut found_erc20 = false;
    for _ in 0..2 {
        match recv(&mut events).await {
            Some(MonitorEvent::PaymentDetected(p)) if p.is_native => {
                assert_eq!(p.invoice_id, native_invoice);
                assert_eq!(p.payment_address, native_address);
                assert_eq!(p.amount, native_amount);
                assert_eq!(p.tx_hash, native_tx);
                found_native = true;
            }
            Some(MonitorEvent::PaymentDetected(p)) => {
                assert_eq!(p.invoice_id, erc20_invoice);
                assert_eq!(p.payment_address, erc20_address);
                assert_eq!(p.token_address, Some(token));
                assert_eq!(p.amount, erc20_amount);
                assert_eq!(p.tx_hash, erc20_tx);
                found_erc20 = true;
            }
            other => panic!("expected a PaymentDetected, got {other:?}"),
        }
    }
    assert!(found_native, "expected the native transfer to be found");
    assert!(found_erc20, "expected the ERC-20 transfer to be found");

    // Nothing else - specifically nothing naming `unlisted_address` - was
    // published.
    assert!(
        recv(&mut events).await.is_none(),
        "the transfer to an address outside `watches` must not be reported"
    );
}

#[tokio::test]
async fn rescan_from_emits_detected_before_confirmed_for_an_already_confirmed_transfer() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let address = Address::random();
    let invoice_id = uuid::Uuid::new_v4();
    let amount = U256::from(1u64);
    let tx_hash = B256::random();

    // The transfer sits at the chain tip, so it has exactly one
    // confirmation - enough to confirm immediately with
    // `required_confirmations: 1`.
    const BLOCK: u64 = 100;

    test_source
        .add_native_transfer(
            BLOCK,
            make_native_transfer(Address::random(), address, amount, tx_hash),
        )
        .await;
    seed_block_hashes(&test_source, BLOCK, BLOCK);
    test_source.set_block_number(BLOCK);

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(1, 10_000),
    ));
    let mut events = monitor.subscribe();

    monitor
        .rescan_from(
            BLOCK,
            vec![RescanWatch {
                address,
                invoice_id,
                token_contract: None,
            }],
        )
        .await
        .expect("rescan within the cap must succeed");

    match recv(&mut events).await {
        Some(MonitorEvent::PaymentDetected(p)) => {
            assert_eq!(p.tx_hash, tx_hash);
            assert_eq!(p.invoice_id, invoice_id);
        }
        other => panic!("expected PaymentDetected first, got {other:?}"),
    }
    match recv(&mut events).await {
        Some(MonitorEvent::PaymentConfirmed(c)) => {
            assert_eq!(c.tx_hash, tx_hash);
            assert_eq!(c.invoice_id, invoice_id);
            assert!(c.confirmations >= 1);
        }
        other => panic!("expected PaymentConfirmed second, got {other:?}"),
    }
}

#[tokio::test]
async fn rescan_from_refuses_a_window_wider_than_the_cap_with_no_partial_scan() {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let test_source = source.clone();

    let address = Address::random();
    let invoice_id = uuid::Uuid::new_v4();

    // Would be found by an unbounded scan: inside [0, tip] but outside the
    // tiny cap below.
    test_source
        .add_native_transfer(
            5,
            make_native_transfer(Address::random(), address, U256::from(1u64), B256::random()),
        )
        .await;
    seed_block_hashes(&test_source, 0, 100);
    test_source.set_block_number(100); // window [0, 100] is 101 blocks

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(1, 10), // cap far narrower than the window
    ));
    let mut events = monitor.subscribe();

    let result = monitor
        .rescan_from(
            0,
            vec![RescanWatch {
                address,
                invoice_id,
                token_contract: None,
            }],
        )
        .await;

    assert!(
        result.is_err(),
        "a window wider than the cap must be refused"
    );
    assert!(
        recv(&mut events).await.is_none(),
        "a refused rescan must not have scanned (and published) any part of the window"
    );
}
