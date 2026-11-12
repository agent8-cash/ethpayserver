//! RPC cost analysis and provider comparison.
//!
//! Provides utilities to collect RPC metrics during monitor operation, project
//! costs across different pricing models, and identify the cheaper provider
//! for a given workload.
//!
//! ## Supported Providers
//!
//! - **Alchemy**: Prices on compute units (CU), free tier 30M CU/month, then $0.52 per million
//! - **QuickNode**: Prices on request count at $0.00003/request, $49/month base fee
//!
//! ## Pricing Source
//!
//! These are the published rates from:
//! - Alchemy: https://www.alchemy.com/pricing (30M free tier, overage $0.52/1M CU)
//! - QuickNode: https://www.quicknode.com/pricing (base $49/month + per-request overage)

use std::collections::HashMap;

/// Pricing information for an RPC provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPricing {
    /// Free tier quota (requests or CU per month, provider-specific).
    pub free_tier_quota: u64,
    /// Cost per unit above free tier ($).
    pub cost_per_unit: f64,
    /// Monthly base fee ($), if any.
    pub base_fee: f64,
    /// Type of unit (requests, compute units, etc).
    pub unit_name: &'static str,
}

impl ProviderPricing {
    /// Alchemy pricing model: 30M CU/month free, then $0.52/1M CU overage, no base fee.
    pub fn alchemy() -> Self {
        Self {
            free_tier_quota: 30_000_000,       // 30M CU/month free
            cost_per_unit: 0.52 / 1_000_000.0, // $0.52 per million CU overage
            base_fee: 0.0,
            unit_name: "compute_units",
        }
    }

    /// QuickNode pricing model: $49/month base fee + $0.00003/request overage.
    pub fn quicknode() -> Self {
        Self {
            free_tier_quota: 0,     // No included requests
            cost_per_unit: 0.00003, // $0.00003 per request
            base_fee: 49.0,         // $49/month base fee
            unit_name: "requests",
        }
    }

    /// Calculate monthly cost given monthly usage.
    ///
    /// Cost = base_fee + max(0, (usage - free_tier) * cost_per_unit)
    pub fn monthly_cost(&self, monthly_usage: u64) -> f64 {
        let overage_cost = if monthly_usage <= self.free_tier_quota {
            0.0
        } else {
            let overage = (monthly_usage - self.free_tier_quota) as f64;
            overage * self.cost_per_unit
        };
        self.base_fee + overage_cost
    }
}

/// Compute unit weight for an RPC method (Alchemy pricing).
pub fn compute_units_for_method(method: &str) -> u64 {
    match method {
        "get_logs" | "getLogs" | "eth_getLogs" => 25,
        "get_block" | "eth_getBlockByNumber" | "eth_getBlockByHash" => 15,
        "get_block_with_txs" => 25,
        "get_balance" | "eth_getBalance" => 10,
        "eth_call" | "call" => 20,
        "get_block_number" | "eth_blockNumber" => 10,
        "get_transaction_count" | "eth_getTransactionCount" => 10,
        "get_code" | "eth_getCode" => 20,
        "eth_getStorageAt" => 10,
        "trace_block" => 100,
        "trace_transaction" => 200,
        _ => 10, // Default for unknown methods
    }
}

/// RPC volume metrics from a collection period.
#[derive(Debug, Clone)]
pub struct RpcVolume {
    /// Requests per method.
    pub method_counts: HashMap<String, u64>,
    /// Total requests.
    pub total_requests: u64,
}

impl RpcVolume {
    /// Create from method counts.
    pub fn new(method_counts: HashMap<String, u64>) -> Self {
        let total_requests = method_counts.values().sum();
        Self {
            method_counts,
            total_requests,
        }
    }

    /// Total compute units (Alchemy model, **unscaled sample sum**).
    /// Note: This is the raw sample; multiply by 30 for a monthly projection.
    pub fn total_compute_units(&self) -> u64 {
        self.method_counts
            .iter()
            .map(|(method, count)| compute_units_for_method(method).saturating_mul(*count))
            .sum()
    }

    /// Total compute units scaled to monthly estimate (sample * 30).
    pub fn total_compute_units_monthly(&self) -> u64 {
        self.total_compute_units().saturating_mul(30)
    }

    /// Total requests scaled to monthly estimate (sample * 30).
    pub fn total_requests_monthly(&self) -> u64 {
        self.total_requests.saturating_mul(30)
    }

    /// Cost on Alchemy for monthly usage based on this sample.
    pub fn alchemy_cost_monthly(&self) -> f64 {
        ProviderPricing::alchemy().monthly_cost(self.total_compute_units_monthly())
    }

    /// Cost on QuickNode for monthly usage based on this sample.
    pub fn quicknode_cost_monthly(&self) -> f64 {
        ProviderPricing::quicknode().monthly_cost(self.total_requests_monthly())
    }

    /// Identify the cheaper provider.
    /// At zero volume, returns Alchemy (it is cheaper or equal).
    pub fn cheaper_provider(&self) -> &'static str {
        let alchemy = self.alchemy_cost_monthly();
        let quicknode = self.quicknode_cost_monthly();
        if alchemy <= quicknode {
            "Alchemy"
        } else {
            "QuickNode"
        }
    }

    /// Savings by choosing the cheaper provider.
    pub fn monthly_savings(&self) -> f64 {
        (self.alchemy_cost_monthly() - self.quicknode_cost_monthly()).abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alchemy_pricing() {
        let alchemy = ProviderPricing::alchemy();
        assert_eq!(alchemy.free_tier_quota, 30_000_000); // 30M CU free
        assert!((alchemy.cost_per_unit - 0.52 / 1_000_000.0).abs() < 0.0001);
        assert_eq!(alchemy.base_fee, 0.0);

        // Within free tier: cost is 0
        assert_eq!(alchemy.monthly_cost(30_000_000), 0.0);

        // 1 million CU over free tier = $0.52
        let cost = alchemy.monthly_cost(31_000_000);
        assert!((cost - 0.52).abs() < 0.001);
    }

    #[test]
    fn test_quicknode_pricing() {
        let quicknode = ProviderPricing::quicknode();
        assert_eq!(quicknode.free_tier_quota, 0);
        assert_eq!(quicknode.base_fee, 49.0);

        // At zero requests, QuickNode costs $49 (base fee only)
        let cost_zero = quicknode.monthly_cost(0);
        assert!((cost_zero - 49.0).abs() < 0.001);

        // 1M requests + $49 base = $49 + $30 = $79
        let cost = quicknode.monthly_cost(1_000_000);
        assert!((cost - 79.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_units_for_methods() {
        assert_eq!(compute_units_for_method("get_logs"), 25);
        assert_eq!(compute_units_for_method("eth_getLogs"), 25);
        assert_eq!(compute_units_for_method("get_block_number"), 10);
        assert_eq!(compute_units_for_method("unknown_method"), 10);
    }

    #[test]
    fn test_rpc_volume_calculation() {
        let mut methods = HashMap::new();
        methods.insert("get_logs".to_string(), 1000u64);
        methods.insert("get_block_number".to_string(), 500u64);

        let volume = RpcVolume::new(methods);
        assert_eq!(volume.total_requests, 1500);

        // 1000 * 25 + 500 * 10 = 30000 CU
        assert_eq!(volume.total_compute_units(), 30_000);
    }

    #[test]
    fn test_cheaper_provider_selection() {
        let mut methods = HashMap::new();
        // Sample of 100k get_logs (raw sample, will be scaled to monthly by * 30)
        methods.insert("get_logs".to_string(), 100_000u64);

        let volume = RpcVolume::new(methods);

        // Scaled to monthly: 100k * 30 = 3M requests
        // Alchemy: 100k * 25 CU/call * 30 = 75M CU (below 30M free tier, but wait - we scale to monthly)
        // Actually: 100k * 25 = 2.5M CU in sample, * 30 = 75M CU monthly
        // Alchemy: 75M - 30M free = 45M over, at $0.52/M = $23.40
        // QuickNode: 3M requests * $0.00003 + $49 base = $90 + $49 = $139

        let alchemy_cost = volume.alchemy_cost_monthly();
        let quicknode_cost = volume.quicknode_cost_monthly();

        println!(
            "100k get_logs sample (scaled to monthly) - Alchemy: ${:.2}, QuickNode: ${:.2}",
            alchemy_cost, quicknode_cost
        );

        // Verify that cheaper_provider() returns the actual cheaper one
        let cheaper = volume.cheaper_provider();
        if cheaper == "Alchemy" {
            assert!(alchemy_cost <= quicknode_cost);
        } else {
            assert!(quicknode_cost < alchemy_cost);
        }
    }
}
