#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]

use std::collections::HashMap;
use types::ChainId;

use evm::monitor::{ChainHealth, SourceStatus};

use super::deep::{build_rpc_map, chains_are_fresh, sentry_release_header, sentry_release_headers};
use super::{
    ChainHealthInfo, DeepHealthResponse, DependencyHealth, MonitorHealth, ReadinessResponse,
    RpcHealth,
};

// -- Response type serialization tests --

#[test]
fn readiness_ready_omits_failing() {
    let resp = ReadinessResponse {
        status: "ready".to_string(),
        failing: vec![],
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["status"], "ready");
    assert!(
        json.get("failing").is_none(),
        "empty failing should be omitted"
    );
}

#[test]
fn readiness_not_ready_includes_failing() {
    let resp = ReadinessResponse {
        status: "not_ready".to_string(),
        failing: vec!["postgres".to_string(), "rpc:56".to_string()],
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["status"], "not_ready");
    let failing = json["failing"].as_array().unwrap();
    assert_eq!(failing.len(), 2);
    assert_eq!(failing[0], "postgres");
    assert_eq!(failing[1], "rpc:56");
}

#[test]
fn deep_health_response_serialization() {
    let mut rpcs = HashMap::new();
    rpcs.insert(
        "1".to_string(),
        RpcHealth {
            status: "ok".to_string(),
            latency_ms: 12,
            last_block: Some(20_000_000),
            error: None,
        },
    );
    rpcs.insert(
        "56".to_string(),
        RpcHealth {
            status: "error".to_string(),
            latency_ms: 1000,
            last_block: None,
            error: Some("disconnected".to_string()),
        },
    );

    let resp = DeepHealthResponse {
        build_sha: "abc1234".to_string(),
        version: "0.1.0".to_string(),
        postgres: DependencyHealth {
            status: "ok".to_string(),
            latency_ms: 3,
            error: None,
        },
        redis: DependencyHealth {
            status: "ok".to_string(),
            latency_ms: 1,
            error: None,
        },
        rpcs,
        monitor: MonitorHealth {
            status: "ok".to_string(),
            data_fresh: true,
        },
        webauthn: Some(api_types::WebAuthnHealth {
            rp_id: "testnet.agent8.cash".to_string(),
            rp_origin: "https://testnet.agent8.cash".to_string(),
        }),
    };

    let json = serde_json::to_value(&resp).unwrap();

    // The deploy check reads these by jq path, so the wire names are the
    // contract - not just the values. This is what replaces scraping the log
    // line, where `rp_id` and `=` arrive in separate ANSI escape sequences and a
    // literal `rp_id=` matches nothing.
    assert_eq!(json["webauthn"]["rp_id"], "testnet.agent8.cash");
    assert_eq!(json["webauthn"]["rp_origin"], "https://testnet.agent8.cash");

    assert_eq!(json["build_sha"], "abc1234");
    assert_eq!(json["version"], "0.1.0");
    assert_eq!(json["postgres"]["status"], "ok");
    assert_eq!(json["postgres"]["latency_ms"], 3);
    assert!(json["postgres"].get("error").is_none());
    assert_eq!(json["redis"]["status"], "ok");
    assert_eq!(json["rpcs"]["1"]["status"], "ok");
    assert_eq!(json["rpcs"]["1"]["last_block"], 20_000_000);
    assert_eq!(json["rpcs"]["56"]["status"], "error");
    assert_eq!(json["rpcs"]["56"]["error"], "disconnected");
    assert!(json["rpcs"]["56"].get("last_block").is_none());
    assert_eq!(json["monitor"]["status"], "ok");
    assert_eq!(json["monitor"]["data_fresh"], true);
}

#[test]
fn deep_health_no_monitor_configured() {
    let resp = DeepHealthResponse {
        build_sha: "dev".to_string(),
        version: "0.1.0".to_string(),
        postgres: DependencyHealth {
            status: "ok".to_string(),
            latency_ms: 2,
            error: None,
        },
        redis: DependencyHealth {
            status: "ok".to_string(),
            latency_ms: 0,
            error: Some("not configured".to_string()),
        },
        rpcs: HashMap::new(),
        monitor: MonitorHealth {
            status: "ok".to_string(),
            data_fresh: false,
        },
        // Absent on purpose here: a payserver without WebAuthn omits it, and the
        // response must still serialise and read correctly.
        webauthn: None,
    };

    let json = serde_json::to_value(&resp).unwrap();

    // Absent, not null: `skip_serializing_if` keeps the key out entirely so an
    // older server and a payserver without WebAuthn produce the same shape.
    assert!(
        json.get("webauthn").is_none(),
        "an absent relying party must not emit a null key"
    );
    assert_eq!(json["build_sha"], "dev");
    assert_eq!(json["redis"]["error"], "not configured");
    assert!(json["rpcs"].as_object().unwrap().is_empty());
    assert_eq!(json["monitor"]["data_fresh"], false);
}

// -- x-sentry-release header --
//
// `SENTRY_RELEASE` and `ETHPAYSERVER_BUILD_SHA` are set by two separate CI
// steps from the same commit sha, so a rename, typo or a later rebuild that
// drops the former would still produce a binary with a correct `build_sha`
// and a compile-time `None` here - which compiles fine and looks identical
// to success. These pin what the header carries in both cases.

#[test]
fn sentry_release_header_passes_through_a_compiled_value() {
    assert_eq!(sentry_release_header(Some("abc1234")), "abc1234");
}

#[test]
fn sentry_release_header_is_empty_not_a_placeholder_when_uncompiled() {
    // Empty, not "unknown" or "dev": a deploy check compares this byte-for-byte
    // against `build_sha`, and a placeholder that happened to match a real sha
    // would defeat the whole point of the comparison.
    assert_eq!(sentry_release_header(None), "");
}

// -- x-evmmonitor-sentry-release header --
//
// evmmonitor is a second binary with the same drift risk as the one above,
// and no HTTP endpoint of its own - its release only reaches this response
// when something upstream actually observed it, so absence has to mean
// "nothing to check" rather than silently comparing against an empty string.

#[test]
fn evmmonitor_header_is_present_when_a_release_was_observed() {
    let headers = sentry_release_headers(Some("abc1234"), Some("def5678".to_string()));
    assert_eq!(headers.get("x-sentry-release").unwrap(), "abc1234");
    assert_eq!(
        headers.get("x-evmmonitor-sentry-release").unwrap(),
        "def5678"
    );
}

#[test]
fn evmmonitor_header_is_absent_not_empty_when_nothing_was_observed() {
    // Not configured, or the Redis-relayed value never arrived: either way,
    // a deploy check has to be able to tell "no evmmonitor to verify" apart
    // from "evmmonitor reported an empty release", which an empty-string
    // header would collapse into the same thing.
    let headers = sentry_release_headers(Some("abc1234"), None);
    assert_eq!(headers.get("x-sentry-release").unwrap(), "abc1234");
    assert!(headers.get("x-evmmonitor-sentry-release").is_none());
}

// -- ChainHealthInfo conversion tests --

#[test]
fn chain_health_connected_conversion() {
    let health = ChainHealth {
        chain_id: 1,
        chain_name: "Ethereum".to_string(),
        status: SourceStatus::Connected,
        current_block: Some(20_000_000),
        last_processed_block: Some(19_999_999),
        watched_addresses: 42,
        is_healthy: true,
    };
    let info = super::models::chain_health_info(health);
    assert_eq!(info.status, "connected");
    assert!(info.is_healthy);
    assert_eq!(info.watched_addresses, Some(42));

    // The conversion has to widen the monitor's EIP-155 number into a CAIP-2
    // identifier. It previously emitted "1", which the client parses as a
    // `ChainId` and rejects - taking the whole chains-health response down
    // rather than one field. This test constructed a `ChainHealth` and never
    // looked at `chain_id`, which is why it passed.
    assert_eq!(info.chain_id, ChainId::parse("eip155:1").unwrap());
}

#[test]
fn chain_health_failed_conversion() {
    let health = ChainHealth {
        chain_id: 56,
        chain_name: "BSC".to_string(),
        status: SourceStatus::Failed("rpc timeout".to_string()),
        current_block: None,
        last_processed_block: None,
        watched_addresses: 0,
        is_healthy: false,
    };
    let info = super::models::chain_health_info(health);
    assert_eq!(info.status, "failed: rpc timeout");
    assert!(!info.is_healthy);
}

#[test]
fn chain_health_disconnected_conversion() {
    let health = ChainHealth {
        chain_id: 137,
        chain_name: "Polygon".to_string(),
        status: SourceStatus::Disconnected,
        current_block: Some(50_000_000),
        last_processed_block: Some(49_999_000),
        watched_addresses: 10,
        is_healthy: false,
    };
    let info = super::models::chain_health_info(health);
    assert_eq!(info.status, "disconnected");
    assert!(!info.is_healthy);
}

#[test]
fn readiness_response_roundtrip() {
    let resp = ReadinessResponse {
        status: "not_ready".to_string(),
        failing: vec!["redis".to_string()],
    };
    let json = serde_json::to_string(&resp).unwrap();
    let deserialized: ReadinessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.status, "not_ready");
    assert_eq!(deserialized.failing, vec!["redis"]);
}

#[test]
fn dependency_health_error_field_omitted_when_none() {
    let dep = DependencyHealth {
        status: "ok".to_string(),
        latency_ms: 5,
        error: None,
    };
    let json = serde_json::to_value(&dep).unwrap();
    assert!(json.get("error").is_none());
}

#[test]
fn rpc_health_last_block_omitted_when_none() {
    let rpc = RpcHealth {
        status: "error".to_string(),
        latency_ms: 1000,
        last_block: None,
        error: Some("timeout".to_string()),
    };
    let json = serde_json::to_value(&rpc).unwrap();
    assert!(json.get("last_block").is_none());
    assert_eq!(json["error"], "timeout");
}

// ===========================================================================
// /health/chains disclosure boundary
//
// Whether a chain is up is public; how far behind it is, and why it failed,
// is not. These pin that line, because the failure mode is silent: adding a
// field to ChainHealthInfo and forgetting to redact it leaks operational
// detail to anonymous callers and nothing goes red.
// ===========================================================================

fn detailed() -> ChainHealthInfo {
    ChainHealthInfo {
        chain_id: ChainId::parse("eip155:11155111").unwrap(),
        chain_name: "Sepolia".to_string(),
        status: "failed: https://eth-sepolia.example.com/v2/SECRET-KEY timed out".to_string(),
        current_block: Some(9_100_200),
        last_processed_block: Some(9_100_150),
        watched_addresses: Some(42),
        is_healthy: false,
    }
}

#[test]
fn redacted_chain_health_keeps_the_on_off_answer() {
    let public = detailed().redact();

    // The whole point of showing this to a merchant.
    assert_eq!(public.chain_id, ChainId::evm(11_155_111));
    assert_eq!(public.chain_name, "Sepolia");
    assert!(!public.is_healthy);
    assert_eq!(public.status, "failed");
}

#[test]
fn redacted_chain_health_drops_operational_detail() {
    let public = detailed().redact();

    assert_eq!(public.current_block, None, "block height is admin-only");
    assert_eq!(
        public.last_processed_block, None,
        "monitor lag is admin-only"
    );
    assert_eq!(
        public.watched_addresses, None,
        "watched address count is admin-only"
    );
}

/// The reason string is the one that actually matters: an RPC failure routinely
/// carries the provider host and an API key, so it must not survive redaction.
#[test]
fn redaction_strips_the_failure_reason_not_just_the_detail_fields() {
    let public = detailed().redact();

    assert!(
        !public.status.contains("SECRET-KEY"),
        "redacted status must not leak the endpoint: {}",
        public.status
    );
    assert!(
        !public.status.contains("example.com"),
        "redacted status must not leak the provider: {}",
        public.status
    );
}

#[test]
fn redaction_leaves_a_healthy_chain_readable() {
    let healthy = ChainHealthInfo {
        status: "connected".to_string(),
        is_healthy: true,
        ..detailed()
    };
    let public = healthy.redact();

    assert_eq!(public.status, "connected");
    assert!(public.is_healthy);
}

// ===========================================================================
// data_fresh must reflect actual freshness, not "the health key parsed".
//
// The bug this guards against: a monitor that stopped processing blocks
// hours ago but is still connected reports a non-empty chains Vec, so
// `!chains.is_empty()` was true the entire time. These pin `data_fresh` to
// the one field that was ever telling the truth: `is_healthy`.
// ===========================================================================

fn chain(chain_id: u64, is_healthy: bool) -> ChainHealth {
    ChainHealth {
        chain_id,
        chain_name: "Sepolia".to_string(),
        status: SourceStatus::Connected,
        current_block: Some(11_702_564),
        last_processed_block: Some(if is_healthy { 11_702_560 } else { 11_699_377 }),
        watched_addresses: 0,
        is_healthy,
    }
}

#[test]
fn empty_chains_are_never_fresh() {
    assert!(
        !chains_are_fresh(&[]),
        "no chains means the check answered nothing"
    );
}

#[test]
fn all_healthy_chains_are_fresh() {
    assert!(chains_are_fresh(&[chain(1, true), chain(11_155_111, true)]));
}

#[test]
fn one_stalled_chain_makes_the_whole_snapshot_stale() {
    // This is the exact shape of the incident: RPC reachable (Connected),
    // current_block advancing, but 3,187 blocks behind. A monitor that only
    // checked "did the list parse" would call this fresh.
    assert!(!chains_are_fresh(&[
        chain(1, true),
        chain(11_155_111, false)
    ]));
}

#[test]
fn connected_but_lagging_reports_the_lag_not_a_bare_unhealthy() {
    let rpcs = build_rpc_map(vec![chain(11_155_111, false)], 5);
    let rpc = &rpcs["11155111"];
    assert_eq!(rpc.status, "error");
    assert_eq!(
        rpc.error.as_deref(),
        Some("connected but 3187 blocks behind")
    );
}

#[test]
fn healthy_chain_reports_no_error() {
    let rpcs = build_rpc_map(vec![chain(1, true)], 5);
    assert_eq!(rpcs["1"].status, "ok");
    assert!(rpcs["1"].error.is_none());
}

#[test]
fn connected_with_no_processed_block_reports_that_not_a_bare_unhealthy() {
    // Connected but neither block number is known yet - the fallback arm of
    // `build_rpc_map`'s Connected match, otherwise never exercised by a test.
    let chain = ChainHealth {
        chain_id: 11_155_111,
        chain_name: "Sepolia".to_string(),
        status: SourceStatus::Connected,
        current_block: None,
        last_processed_block: None,
        watched_addresses: 0,
        is_healthy: false,
    };
    let rpcs = build_rpc_map(vec![chain], 5);
    let rpc = &rpcs["11155111"];
    assert_eq!(rpc.status, "error");
    assert_eq!(
        rpc.error.as_deref(),
        Some("connected but has not processed a block yet")
    );
}
