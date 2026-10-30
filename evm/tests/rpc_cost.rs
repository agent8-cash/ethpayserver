//! How many RPC calls payment detection makes, as watched addresses grow.
//!
//! Every other test in this crate asks whether a payment was found. None asks
//! how many requests finding it takes — and that shape, calls-per-block
//! against addresses-watched, is what decides whether watching a chain scales.
//!
//! Both halves are now O(1) per block. ERC20 detection puts every watched
//! address into one `eth_getLogs` filter; native detection reads the block
//! once and matches its transfers locally. Neither grows with the number of
//! open invoices.
//!
//! Native detection used to be O(N): one `eth_getBalance` per watched address
//! per block, to find which balances had grown, before reading the block
//! anyway to attribute them. These tests are what made that visible and what
//! keep it from coming back.
//!
//! They pin call counts, not correctness, and state the shape rather than a
//! number an implementation could change innocently.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use evm::monitor::{
    ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent, WatchedAddress, make_block,
};
use evm::{Address, U256};

const TEST_CHAIN_ID: u64 = 11155111;

fn test_chain_config() -> &'static evm::ChainConfig {
    evm::get_any_chain_config(TEST_CHAIN_ID).expect("Sepolia config exists")
}

fn test_monitor_config() -> ChainMonitorConfig {
    ChainMonitorConfig {
        required_confirmations: 3,
        max_blocks_per_scan: 100,
        confirmation_check_interval_secs: 60,
        stall_timeout_secs: 120,
        loop_hang_timeout_secs: 300,
        monitor_native: true,
        monitor_erc20: true,
        rescan_max_blocks: 10_000,
    }
}

/// What one block cost, in RPC calls, with `n` addresses watched.
#[derive(Debug, Default, PartialEq, Eq)]
struct BlockCalls {
    /// `find_native_transfers_to` — reads the block once, matches locally.
    block_reads: u64,
    /// `eth_getBalance` — should be zero; native detection no longer polls.
    balance_polls: u64,
    /// `eth_getLogs` — one filter covering every watched address.
    log_queries: u64,
}

/// Start a monitor watching `n` addresses, feed it one block, and report how
/// many times each RPC method was called while processing it.
///
/// Counts are reset after startup and before the block is pushed, so what is
/// measured is *one block* rather than the monitor booting.
async fn calls_for_one_block(n: usize, token: Option<Address>) -> BlockCalls {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let probe = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(),
    ));

    for _ in 0..n {
        monitor
            .watch(WatchedAddress {
                address: Address::random(),
                invoice_id: uuid::Uuid::new_v4(),
                expected_amount: Some(U256::from(1u64)),
                token_contract: token,
                created_at: Utc::now(),
            })
            .await;
    }

    let mut events = monitor.subscribe();
    let running = Arc::clone(&monitor);
    let handle = tokio::spawn(async move { running.start().await });

    let started = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert!(
        matches!(started, Ok(Ok(MonitorEvent::MonitorStarted { .. }))),
        "monitor did not start: {started:?}"
    );

    probe.reset_call_counts();
    probe.push_block(make_block(100));

    // A block with no payment emits no event, so this waits on the observable
    // side effect instead: the calls the block produced.
    //
    // Waits for the counts to stop changing rather than for the first call to
    // land. `record_call` fires on *entry*, so a snapshot taken the moment the
    // first call is seen is read while `process_block` may still be running -
    // and a regression that adds calls *after* the first one would only be
    // caught by whatever slack the poll interval happened to leave. For a test
    // whose whole job is stopping the old shape creeping back, that is the
    // wrong thing to depend on.
    let mut stable_for = 0;
    let mut previous = (0, 0, 0);
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let now = (
            probe.call_count("find_native_transfers_to"),
            probe.call_count("get_balance"),
            probe.call_count("get_logs"),
        );
        if now == previous && now != (0, 0, 0) {
            stable_for += 1;
            // Three consecutive identical reads, i.e. ~30ms with nothing new.
            if stable_for >= 3 {
                break;
            }
        } else {
            stable_for = 0;
            previous = now;
        }
    }

    let calls = BlockCalls {
        block_reads: probe.call_count("find_native_transfers_to"),
        balance_polls: probe.call_count("get_balance"),
        log_queries: probe.call_count("get_logs"),
    };
    handle.abort();
    calls
}

/// Native detection: one block read per block, at any scale.
///
/// The property that matters is that this does **not** grow with watched
/// addresses. It used to: one `eth_getBalance` per address per block, so a
/// chain with many open invoices paid for every one of them on every block.
///
/// Pinned at exactly one call, and at zero balance polls, so the old shape
/// cannot creep back without someone noticing.
#[tokio::test]
async fn native_detection_costs_one_call_per_block_at_any_scale() {
    let mut measured = Vec::new();
    for n in [1usize, 8, 64] {
        measured.push((n, calls_for_one_block(n, None).await));
    }

    for (n, calls) in &measured {
        assert_eq!(
            calls.block_reads, 1,
            "one block should cost exactly one block read however many addresses are \
             watched; {n} addresses produced {}. Measured: {measured:?}",
            calls.block_reads
        );
        assert_eq!(
            calls.balance_polls, 0,
            "native detection should not poll balances at all; {n} addresses produced \
             {}. Measured: {measured:?}",
            calls.balance_polls
        );
    }
}

/// ERC20 detection: exactly one `eth_getLogs` per block, at any scale.
///
/// Unchanged, and kept as the other half of the pair: both detection paths
/// now answer the same way, which is the property worth protecting.
#[tokio::test]
async fn erc20_detection_costs_one_call_per_block_at_any_scale() {
    let token = Address::random();
    let mut measured = Vec::new();
    for n in [1usize, 8, 64] {
        measured.push((n, calls_for_one_block(n, Some(token)).await.log_queries));
    }

    for (n, calls) in &measured {
        assert_eq!(
            *calls, 1,
            "one block should cost exactly one get_logs however many addresses are \
             watched; {n} addresses produced {calls}. Measured: {measured:?}"
        );
    }
}
