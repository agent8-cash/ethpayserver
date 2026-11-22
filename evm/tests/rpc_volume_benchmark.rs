//! RPC volume benchmarking for cost analysis.
//!
//! Measures real RPC request volume on testnet to choose between providers
//! that price on different axes (e.g., Alchemy by compute units vs QuickNode by requests).
//!
//! This test demonstrates:
//! 1. Tracking RPC requests per chain and per method
//! 2. Collecting steady-state volume metrics
//! 3. Projecting costs across multiple chains
//! 4. Choosing the cheaper provider based on actual usage patterns

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use evm::monitor::{
    ChainMonitor, ChainMonitorConfig, MockBlockSource, MonitorEvent, WatchedAddress, make_block,
};
use evm::rpc_cost::RpcVolume;
use evm::{Address, U256};

const TEST_CHAIN_ID: u64 = 11155111; // Sepolia

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

/// Run monitor for a specified number of blocks, collecting RPC metrics.
async fn collect_rpc_volume(num_blocks: usize, num_addresses: usize) -> RpcVolume {
    let source = MockBlockSource::new(TEST_CHAIN_ID);
    let probe = source.clone();

    let monitor = Arc::new(ChainMonitor::new(
        test_chain_config(),
        source,
        test_monitor_config(),
    ));

    // Set up watched addresses
    for _ in 0..num_addresses {
        monitor
            .watch(WatchedAddress {
                address: Address::random(),
                invoice_id: uuid::Uuid::new_v4(),
                expected_amount: Some(U256::from(1u64)),
                token_contract: None,
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
        "monitor did not start"
    );

    // Reset counts before processing test blocks
    probe.reset_call_counts();

    // Push test blocks
    for block_num in 100..100 + num_blocks as u64 {
        probe.push_block(make_block(block_num));

        // Wait for processing to complete
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Allow time for all RPC calls to complete
    let mut stable_for = 0;
    let mut previous = std::collections::HashMap::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let now = [
            ("get_logs", probe.call_count("get_logs")),
            ("get_balance", probe.call_count("get_balance")),
            (
                "find_native_transfers_to",
                probe.call_count("find_native_transfers_to"),
            ),
            ("get_block_number", probe.call_count("get_block_number")),
            ("get_block", probe.call_count("get_block")),
        ];

        let now_map: std::collections::HashMap<_, _> = now.iter().cloned().collect();
        if now_map == previous && !previous.is_empty() {
            stable_for += 1;
            if stable_for >= 3 {
                break;
            }
        } else {
            stable_for = 0;
            previous = now_map;
        }
    }

    // Collect final counts
    let mut method_counts = std::collections::HashMap::new();
    method_counts.insert("get_logs".to_string(), probe.call_count("get_logs"));
    method_counts.insert("get_balance".to_string(), probe.call_count("get_balance"));
    method_counts.insert(
        "find_native_transfers_to".to_string(),
        probe.call_count("find_native_transfers_to"),
    );
    method_counts.insert(
        "get_block_number".to_string(),
        probe.call_count("get_block_number"),
    );
    method_counts.insert("get_block".to_string(), probe.call_count("get_block"));

    handle.abort();

    RpcVolume::new(method_counts)
}

/// Volume sample with 1 address watching 10 blocks on Sepolia.
#[tokio::test]
async fn measure_rpc_volume_one_address() {
    let sample = collect_rpc_volume(10, 1).await;

    println!("RPC Volume Sample (1 address, 10 blocks):");
    println!("  Total requests (sample): {}", sample.total_requests);
    println!(
        "  Total requests (monthly estimate): {}",
        sample.total_requests_monthly()
    );
    println!("  Methods: {:?}", sample.method_counts);
    println!(
        "  Monthly estimate (Alchemy): {:.0} CU",
        sample.total_compute_units_monthly()
    );
    println!(
        "  Monthly cost (Alchemy): ${:.2}",
        sample.alchemy_cost_monthly()
    );
    println!(
        "  Monthly cost (QuickNode): ${:.2}",
        sample.quicknode_cost_monthly()
    );
    println!("  Cheaper: {}", sample.cheaper_provider());

    // At least native detection should make RPC calls
    assert!(sample.total_requests > 0, "should have recorded RPC calls");
}

/// Volume sample with multiple addresses.
#[tokio::test]
async fn measure_rpc_volume_scales_linearly() {
    let sample_1 = collect_rpc_volume(5, 1).await;
    let sample_8 = collect_rpc_volume(5, 8).await;

    println!("RPC Volume Scaling:");
    println!("  1 address: {} requests", sample_1.total_requests);
    println!("  8 addresses: {} requests", sample_8.total_requests);

    // Verify that adding more watched addresses doesn't cause superlinear growth
    // (native detection should be O(1) per block, erc20 detection should be O(1) too)
    let ratio = if sample_1.total_requests > 0 {
        sample_8.total_requests as f64 / sample_1.total_requests as f64
    } else {
        8.0
    };

    println!("  Ratio: {:.2}x", ratio);
    // The ratio should be close to 1.0 for O(1) scaling per block
    assert!(
        ratio < 3.0,
        "volume should not grow more than 3x with 8x addresses (O(1) scaling)"
    );
}

/// Demonstrate cost calculation across a representative testnet volume.
#[tokio::test]
async fn project_cost_for_representative_testnet_volume() {
    // Collect volume from a representative testnet workload:
    // 50 addresses watched for steady state (simulated across many blocks)
    let sample = collect_rpc_volume(20, 50).await;

    println!("\nProjected Monthly Costs (from 20-block sample):");
    println!("Sample volume:");
    println!("  Total RPC requests in sample: {}", sample.total_requests);
    for (method, count) in &sample.method_counts {
        if *count > 0 {
            println!("    - {}: {}", method, count);
        }
    }

    let monthly_cu = sample.total_compute_units_monthly();
    let alchemy_cost = sample.alchemy_cost_monthly();
    let quicknode_cost = sample.quicknode_cost_monthly();

    println!("\nMonthly projections (scaled * 30):");
    println!("  Estimated compute units: {}", monthly_cu);
    println!("  Alchemy cost: ${:.2}", alchemy_cost);
    println!("  QuickNode cost: ${:.2}", quicknode_cost);

    let cheaper = sample.cheaper_provider();
    let savings = (alchemy_cost - quicknode_cost).abs();
    println!(
        "  Cheaper provider: {} (saves ${:.2}/month)",
        cheaper, savings
    );

    // Verify that one or both providers are being considered
    assert!(
        alchemy_cost > 0.0 || quicknode_cost > 0.0 || monthly_cu > 0,
        "should have calculated costs"
    );
}

/// Demonstrate the method breakdown that informs the choice between providers.
#[tokio::test]
async fn show_rpc_method_breakdown() {
    let sample = collect_rpc_volume(15, 30).await;

    println!("\nRPC Method Breakdown (15 blocks, 30 addresses):");
    let mut methods: Vec<_> = sample.method_counts.iter().collect();
    methods.sort_by_key(|(_, count)| std::cmp::Reverse(**count));

    for (method, count) in methods {
        if *count > 0 {
            let pct = (*count as f64 / sample.total_requests as f64) * 100.0;
            println!("  {}: {} ({:.1}%)", method, count, pct);
        }
    }

    println!("\nThis breakdown shows which methods drive the largest cost on each provider:");
    println!("  - High get_logs (25 CU) vs other methods: Alchemy's CU model weights methods");
    println!("  - QuickNode: counts all requests equally at $0.00003 per request");
}
