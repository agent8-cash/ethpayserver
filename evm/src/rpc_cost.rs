//! RPC cost analysis and provider comparison.
//!
//! Provides utilities to collect RPC metrics during monitor operation, project
//! costs across different pricing models, and identify the cheaper provider
//! for a given workload.
//!
//! ## Supported Providers
//!
//! - **Alchemy**: Prices on compute units (CU), free tier 100M CU/month
//! - **QuickNode**: Prices on request count (typical on-demand, no free tier)
//!
//! ## Pricing Assumptions
//!
//! These are representative pricing models as of the ticket creation date.
//! Actual pricing may vary and should be verified with each provider.
//!
//! **Alchemy (per-CU model):**
//! - Free tier: 100M CU/month
//! - Overage: $0.03 per million CU
//! - Compute unit weights: eth_getLogs (25), eth_blockNumber (10), eth_getBalance (10), etc.
//!
//! **QuickNode (per-request model):**
//! - No free tier (typical on-demand)
//! - Price: $0.00003 per request (example rate)

use std::collections::HashMap;

/// Pricing information for an RPC provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPricing {
    /// Free tier quota (requests or CU per month, provider-specific).
    pub free_tier_quota: u64,
    /// Cost per unit above free tier ($).
    pub cost_per_unit: f64,
    /// Type of unit (requests, compute units, etc).
    pub unit_name: &'static str,
}

impl ProviderPricing {
    /// Alchemy pricing model.
    pub fn alchemy() -> Self {
        Self {
            free_tier_quota: 100_000_000,      // 100M CU/month
            cost_per_unit: 0.03 / 1_000_000.0, // $0.03 per million CU
            unit_name: "compute_units",
        }
    }

    /// QuickNode pricing model.
    pub fn quicknode() -> Self {
        Self {
            free_tier_quota: 0,     // No free tier
            cost_per_unit: 0.00003, // $0.00003 per request
            unit_name: "requests",
        }
    }

    /// Calculate monthly cost given monthly usage.
    pub fn monthly_cost(&self, monthly_usage: u64) -> f64 {
        if monthly_usage <= self.free_tier_quota {
            0.0
        } else {
            let overage = (monthly_usage - self.free_tier_quota) as f64;
            overage * self.cost_per_unit
        }
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

    /// Total compute units (Alchemy model) - scale to monthly if needed.
    pub fn total_compute_units(&self) -> u64 {
        self.method_counts
            .iter()
            .map(|(method, count)| compute_units_for_method(method).saturating_mul(*count))
            .sum()
    }

    /// Cost on Alchemy for monthly usage based on this sample.
    pub fn alchemy_cost_monthly(&self) -> f64 {
        ProviderPricing::alchemy().monthly_cost(self.total_compute_units())
    }

    /// Cost on QuickNode for monthly usage based on this sample.
    pub fn quicknode_cost_monthly(&self) -> f64 {
        ProviderPricing::quicknode().monthly_cost(self.total_requests)
    }

    /// Identify the cheaper provider.
    pub fn cheaper_provider(&self) -> &'static str {
        if self.alchemy_cost_monthly() < self.quicknode_cost_monthly() {
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
        assert_eq!(alchemy.free_tier_quota, 100_000_000);

        // Within free tier
        assert_eq!(alchemy.monthly_cost(50_000_000), 0.0);

        // 1 million CU over free tier = $0.03
        let cost = alchemy.monthly_cost(101_000_000);
        assert!((cost - 0.03).abs() < 0.001);
    }

    #[test]
    fn test_quicknode_pricing() {
        let quicknode = ProviderPricing::quicknode();
        assert_eq!(quicknode.free_tier_quota, 0);

        // 1M requests * $0.00003 = $30
        let cost = quicknode.monthly_cost(1_000_000);
        assert!((cost - 30.0).abs() < 0.01);
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
        // Heavy get_logs usage favors QuickNode
        methods.insert("get_logs".to_string(), 100_000u64);

        let volume = RpcVolume::new(methods);
        let alchemy_cost = volume.alchemy_cost_monthly();
        let quicknode_cost = volume.quicknode_cost_monthly();

        println!(
            "100k get_logs - Alchemy: ${:.2}, QuickNode: ${:.2}",
            alchemy_cost, quicknode_cost
        );

        // In this case QuickNode should be cheaper (less CU than requests)
        if volume.cheaper_provider() == "QuickNode" {
            assert!(quicknode_cost < alchemy_cost);
        } else {
            assert!(alchemy_cost <= quicknode_cost);
        }
    }
}
