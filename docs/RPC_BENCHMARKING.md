# RPC Volume Benchmarking

Measure RPC request volume on testnet to choose between providers that price on different axes (Alchemy on compute units vs QuickNode on request count).

## Overview

This document describes the RPC benchmarking infrastructure added for AG8-952. The goal is to collect real RPC usage metrics during monitor operation and project costs across mainnet chains to identify the cost-optimal provider.

## Quick Start

### Run the Benchmarking Tests

The primary way to measure RPC volume is through the test suite:

```bash
cargo test --test rpc_volume_benchmark -- --nocapture
```

This runs four test scenarios:
1. **measure_rpc_volume_one_address** - Single address, 10 blocks
2. **measure_rpc_volume_scales_linearly** - Verifies O(1) scaling with more addresses
3. **project_cost_for_representative_testnet_volume** - 50 addresses, 20 blocks
4. **show_rpc_method_breakdown** - Displays RPC method composition

### Interpret Results

Output shows:
- Total RPC requests by method
- Monthly estimate in compute units (Alchemy)
- Monthly costs for both providers
- Recommended provider based on cost
- Monthly savings

Example output (from 1-address, 10-block sample scaled to monthly):
```
RPC Volume Sample (1 address, 10 blocks):
  Total requests (sample): 11
  Total requests (monthly estimate): 330
  Methods: {"get_logs": 1, "find_native_transfers_to": 10}
  Monthly estimate (Alchemy): 400 CU
  Monthly cost (Alchemy): $0.00
  Monthly cost (QuickNode): $49.00
  Cheaper: Alchemy
```

## Architecture

### Core Modules

#### `evm::rpc_cost`

Provides utilities for cost calculation and provider comparison:

- **`ProviderPricing`** - Pricing models for Alchemy and QuickNode
- **`RpcVolume`** - Tracks RPC requests by method and calculates costs
- **`compute_units_for_method`** - Maps RPC methods to Alchemy compute unit weights

Key methods:
- `alchemy_cost_monthly()` - Project monthly cost on Alchemy
- `quicknode_cost_monthly()` - Project monthly cost on QuickNode
- `cheaper_provider()` - Select the cost-optimal provider

#### `evm/tests/rpc_volume_benchmark.rs`

Integration tests demonstrating:
- RPC volume measurement during monitor operation
- Cost projection from sample data
- Linear scaling verification
- Method breakdown analysis

### RPC Method Weights (Alchemy CU Model)

Default compute unit weights for common RPC methods:
- `eth_getLogs`: 25 CU
- `eth_call`: 20 CU
- `get_block_with_txs`: 25 CU
- `eth_getBlockByNumber`: 15 CU
- `get_code`: 20 CU
- `trace_block`: 100 CU
- `trace_transaction`: 200 CU
- Other methods: 10 CU (default)

## Pricing Models

### Alchemy (Compute Unit Model)

- **Free Tier**: 30M CU/month
- **Overage**: $0.52 per million CU
- **Unit**: Compute units weighted by method complexity

### QuickNode (Request Count Model)

- **Base Fee**: $49/month
- **Overage**: $0.00003 per request
- **Unit**: Total RPC requests

## Usage Patterns

### For Development

Run the test suite to verify RPC volume measurement is working correctly:

```bash
cargo test --test rpc_volume_benchmark measure_rpc_volume_one_address -- --nocapture
```

### For Production Benchmarking

To collect real RPC volume data on mainnet:

1. **Deploy monitor to production** with production RPC endpoints
2. **Run for 7 days** to collect steady-state data
3. **Export metrics** from monitoring system (Prometheus/Sentry)
4. **Analyze results** using `RpcVolume::new()` with collected method counts
5. **Project costs** using `alchemy_cost_monthly()` and `quicknode_cost_monthly()`

Example analysis code:
```rust
use evm::rpc_cost::RpcVolume;
use std::collections::HashMap;

// Collect method counts from production metrics
let mut methods = HashMap::new();
methods.insert("get_logs".to_string(), 5_000_000u64);
methods.insert("find_native_transfers_to".to_string(), 2_000_000u64);
methods.insert("get_block_number".to_string(), 1_000_000u64);

let volume = RpcVolume::new(methods);

println!("Alchemy cost: ${:.2}", volume.alchemy_cost_monthly());
println!("QuickNode cost: ${:.2}", volume.quicknode_cost_monthly());
println!("Cheaper: {}", volume.cheaper_provider());
```

## Projecting to Mainnet

The benchmarking results from testnet can be scaled to mainnet by:

1. **Block time differences** - Adjust for different block rates per chain
   - Sepolia: 12s blocks
   - Ethereum mainnet: 12s blocks (same)
   - Polygon: 2s blocks (6x more blocks)
   - Arbitrum: 0.25s blocks (48x more blocks)

2. **Address count** - Scale with expected number of merchant payment addresses
   - Monitor per-address overhead is O(1) per block
   - Adding more addresses only increases storage, not RPC volume

3. **Multi-chain calculation** - Sum costs across all supported chains
   - Ethereum, Polygon, Arbitrum, Optimism, Base, Avalanche
   - Each may have different volume patterns

## Future Enhancements

1. **Live metrics export** - Add Prometheus exporter for real-time cost tracking
2. **Provider API integration** - Pull actual pricing from provider APIs
3. **Cost-per-method analysis** - Break down which methods drive highest cost
4. **Alerting** - Alert when projected cost exceeds budget
5. **A/B testing** - Run against both providers simultaneously during pilot

## Notes

- Pricing models per AG8-952 ticket (Alchemy: 30M CU free tier + $0.52/M; QuickNode: $49/month + $0.00003/request). Verify current pricing with providers.
- This benchmarking focuses on RPC call volume; other costs (storage, bandwidth) are not included.
- The monthly scaling (sample × 30) assumes a constant volume pattern; real usage may vary by time of day.
- Costs scale linearly with address count (O(1) monitoring overhead per block).

## Testing Methodology

The test suite uses a mock RPC block source that simulates realistic monitor behavior:

1. Sets up a monitor watching N random addresses
2. Feeds the monitor M blocks
3. Tracks RPC method calls via `MockBlockSource::call_count()`
4. Collects method counts and calculates costs

This approach verifies:
- RPC calls are being made in expected patterns
- Cost calculations are accurate
- Scaling behavior is linear with addresses (O(1) per block)

## References

- Alchemy pricing: https://www.alchemy.com/pricing
- QuickNode pricing: https://www.quicknode.com/pricing
- Alloy provider: https://docs.rs/alloy/

## Maintenance

When updating this implementation:

1. **Update compute unit weights** if Alchemy pricing changes
2. **Update provider pricing** if free tiers or costs change
3. **Add new methods** to `compute_units_for_method()` as monitor gains capabilities
4. **Expand test scenarios** for new blockchain support

For implementation changes, ensure:
- All RPC methods are instrumented with `rpc_metrics::timed_rpc()`
- Tests continue to verify linear scaling
- Cost calculations remain accurate
