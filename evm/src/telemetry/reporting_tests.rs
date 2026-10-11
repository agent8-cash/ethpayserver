use std::sync::Arc;

use super::tests::captured_logs;
use super::*;

/// Companion to the test above: proves `sentry_log_event_filter`'s
/// threshold is actually consulted when wired into a live
/// `sentry_tracing` layer, not just when `apply_log_level_gate` is called
/// directly. Without this, a builder quirk that silently ignores
/// `.event_filter(...)` — so every record keeps `sentry_tracing`'s
/// default filtering regardless of `SENTRY_LOG_LEVEL` — would pass every
/// other test in this file.
#[test]
fn sentry_log_event_filter_suppresses_a_record_below_the_threshold_through_a_real_subscriber() {
    use tracing_subscriber::prelude::*;

    let _dispatcher = tracing_subscriber::registry()
        .with(sentry_tracing::layer().event_filter(sentry_log_event_filter(tracing::Level::ERROR)))
        .set_default();

    let envelopes = sentry::test::with_captured_envelopes_options(
        || {
            tracing::info!("should not reach Sentry logs when min_level=ERROR");
        },
        client_options(None, None, "test".to_string(), 0.0),
    );

    let logs = captured_logs(&envelopes);
    assert!(
        logs.is_empty(),
        "an INFO record should not become a Sentry log when \
         sentry_log_event_filter is wired with min_level=ERROR: {logs:?}"
    );
}

/// Regression test for a composition bug a review pass caught: a bare
/// `.with(filter)` layer sits in the same `Layered` stack as every other
/// layer, and `Layered::enabled` ANDs across all of them — so an event
/// the `LOG_LEVEL` filter rejects would never reach the Sentry layer's
/// `on_event` at all, making `SENTRY_LOG_LEVEL` only ever a *further*
/// restriction on top of `LOG_LEVEL`, never independent of it, exactly
/// contradicting the comment above the call sites in `server.rs` and
/// `evmmonitor/main.rs`. This builds that real stack — a strict
/// `LOG_LEVEL` filter per-layer-filtered onto the fmt layer, and
/// `sentry_log_event_filter` per-layer-filtered onto the Sentry layer,
/// the fix for that bug — and proves an INFO record still reaches Sentry
/// even though the sibling fmt layer's filter would drop it.
#[test]
fn sentry_log_event_filter_is_independent_of_the_log_level_filter_in_the_real_stack() {
    use tracing_subscriber::prelude::*;

    let _dispatcher = tracing_subscriber::registry()
        .with(
            sentry_tracing::layer()
                .event_filter(sentry_log_event_filter(tracing::Level::INFO))
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(tracing_subscriber::EnvFilter::new("error")),
        )
        .set_default();

    let envelopes = sentry::test::with_captured_envelopes_options(
        || {
            tracing::info!(
                "should reach Sentry logs even though the sibling LOG_LEVEL=error filter would drop it"
            );
        },
        client_options(None, None, "test".to_string(), 0.0),
    );

    let logs = captured_logs(&envelopes);
    assert!(
        !logs.is_empty(),
        "an INFO record should reach Sentry's structured logs even when a \
         sibling layer's LOG_LEVEL filter is stricter (error) — SENTRY_LOG_LEVEL \
         must be independent of LOG_LEVEL, not a further restriction on top of it"
    );
}

#[test]
fn resolve_sentry_log_level_defaults_to_warn_when_unset_or_invalid() {
    use tracing_subscriber::prelude::*;

    struct CapturesWarn(Arc<std::sync::Mutex<bool>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturesWarn {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                *self.0.lock().unwrap() = true;
            }
        }
    }

    let previous = std::env::var("SENTRY_LOG_LEVEL").ok();

    // SAFETY: no other test reads or writes SENTRY_LOG_LEVEL.
    unsafe {
        std::env::remove_var("SENTRY_LOG_LEVEL");
    }
    assert_eq!(resolve_sentry_log_level(), tracing::Level::WARN);

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_LOG_LEVEL", "not-a-level");
    }
    // An unparsable value (as opposed to an absent one) is a
    // misconfiguration and must be visible, not silently identical to a
    // deliberate WARN.
    let saw_warn = Arc::new(std::sync::Mutex::new(false));
    let subscriber = tracing_subscriber::registry().with(CapturesWarn(Arc::clone(&saw_warn)));
    tracing::subscriber::with_default(subscriber, || {
        assert_eq!(resolve_sentry_log_level(), tracing::Level::WARN);
    });
    assert!(
        *saw_warn.lock().unwrap(),
        "expected a WARN-level log when SENTRY_LOG_LEVEL is set but unparsable"
    );

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_LOG_LEVEL", "info");
    }
    assert_eq!(resolve_sentry_log_level(), tracing::Level::INFO);

    // SAFETY: see above.
    unsafe {
        match &previous {
            Some(value) => std::env::set_var("SENTRY_LOG_LEVEL", value),
            None => std::env::remove_var("SENTRY_LOG_LEVEL"),
        }
    }
}

#[test]
fn apply_log_level_gate_strips_log_flag_only_below_threshold() {
    use sentry_tracing::EventFilter;

    let full = EventFilter::Breadcrumb | EventFilter::Log;

    // At min_level=WARN: ERROR and WARN keep the Log flag, INFO/DEBUG/TRACE lose it.
    for level in [tracing::Level::ERROR, tracing::Level::WARN] {
        assert!(
            apply_log_level_gate(full, level, tracing::Level::WARN).contains(EventFilter::Log),
            "{level:?} should keep the Log flag at min_level=WARN"
        );
    }
    for level in [
        tracing::Level::INFO,
        tracing::Level::DEBUG,
        tracing::Level::TRACE,
    ] {
        assert!(
            !apply_log_level_gate(full, level, tracing::Level::WARN).contains(EventFilter::Log),
            "{level:?} should lose the Log flag at min_level=WARN"
        );
    }

    // Non-Log flags are untouched either way.
    assert!(
        apply_log_level_gate(full, tracing::Level::INFO, tracing::Level::WARN)
            .contains(EventFilter::Breadcrumb)
    );

    // At min_level=INFO, INFO now keeps the Log flag too.
    assert!(
        apply_log_level_gate(full, tracing::Level::INFO, tracing::Level::INFO)
            .contains(EventFilter::Log)
    );
}

#[test]
fn scrub_event_drops_request_user_and_server_name() {
    let event = Event {
        request: Some(sentry::protocol::Request::default()),
        user: Some(sentry::protocol::User::default()),
        server_name: Some("payserver-prod-01".into()),
        ..Default::default()
    };

    let scrubbed = scrub_event(event).expect("event passes through");
    assert!(scrubbed.request.is_none());
    assert!(scrubbed.user.is_none());
    assert!(scrubbed.server_name.is_none());
}

#[test]
fn scrub_event_redacts_message_and_extra() {
    let mut event = Event {
        message: Some(
            "panic: invalid key \
             0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                .to_string(),
        ),
        ..Default::default()
    };
    event.extra.insert(
        "ctx".to_string(),
        Value::String("token=sk_live_supersecret".to_string()),
    );

    let scrubbed = scrub_event(event).expect("event passes through");
    assert!(!scrubbed.message.unwrap().contains("deadbeefdead"));
    let extra = scrubbed.extra.get("ctx").and_then(Value::as_str).unwrap();
    assert!(!extra.contains("supersecret"), "extra leaked: {extra}");
}

/// Asserts the dependency-free scanners in payserver-commons `scrub` produce
/// byte-identical output to the regex table above, across a corpus chosen to
/// hit every rule plus the boundaries between them (rule ordering, adjacent
/// matches, non-ASCII neighbours, empty and truncated inputs).
///
/// This is the only place both can be compiled. If it fails, the two
/// implementations have diverged and the browser is redacting differently
/// from the servers — fix `scrub`, do not delete the case.
#[test]
fn parity_with_shared_scrubber() {
    const CORPUS: &[&str] = &[
        "",
        "plain message with no secrets",
        "failed to fetch /api/invoices/inv_001: HTTP 502",
        "auth failed for eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc-_123 (401)",
        "eyJ.a.b eyJa..b eyJa.b.c",
        "0x742d35Cc6634C0532925a3b844Bc454e4438f44e paid 0x1234",
        "tx deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef mined",
        "zzdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefzz",
        "GET https://eth-mainnet.g.alchemy.com/v2/9f8e7d6c5b4a3210zz failed",
        "wss://polygon-mainnet.infura.io:443/ws/v3/0123456789abcdefzz closed",
        "https://api.coingecko.com/api/v3/simple/price?ids=ethereum",
        "https://telemetry.example.com/api/random.cash/envelope/",
        "no store for merchant@example.com or a@b.co.uk or bad@b.c",
        "Authorization: Bearer rc_live_opaque123",
        "authorization=eyJhbGciOiJIUzI1NiJ9.eyJhIjoxfQ.sig; token: abc, secret = \"s3cr3t\"",
        "api-key: k1 apikey:k2 API_KEY = k3 private-key k4 passwd:\tk5",
        "secretariat: not a secret keyword",
        "seed abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        "twelve plain lowercase words here really do trip the mnemonic rule ok now",
        "façade ↔ merchant@example.com ↔ 0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
        "token",
        "token=",
    ];
    for input in CORPUS {
        assert_eq!(
            redact_secrets(input),
            scrub::redact_secrets(input),
            "shared `scrub` diverged from the audited regex table on: {input}"
        );
    }
}

#[test]
fn no_dsn_is_disabled_but_permitted_outside_mainnet() {
    assert_eq!(
        reporting_status(false, "testnet"),
        ReportingStatus::DisabledPermitted
    );
    assert_eq!(
        reporting_status(false, "dev"),
        ReportingStatus::DisabledPermitted
    );
}

#[test]
fn no_dsn_in_mainnet_is_refused() {
    assert_eq!(
        reporting_status(false, "mainnet"),
        ReportingStatus::DisabledRefused
    );
}

#[test]
fn no_dsn_with_unset_or_unrecognised_environment_fails_closed() {
    // The exact shape of the incident this exists for: nothing said the
    // environment was wrong, so an absent or misspelled
    // `SENTRY_ENVIRONMENT` must not be treated as a known-safe one.
    for environment in ["", "mainet", "prod", "MAINNET"] {
        assert_eq!(
            reporting_status(false, environment),
            ReportingStatus::DisabledRefused,
            "environment={environment:?} should fail closed"
        );
    }
}

#[test]
fn a_configured_dsn_flips_every_environment_to_enabled() {
    for environment in ["mainnet", "testnet", "dev"] {
        assert_eq!(
            reporting_status(true, environment),
            ReportingStatus::Enabled,
            "environment={environment}"
        );
    }
}

#[test]
fn report_reporting_status_is_ok_when_permitted_and_err_when_refused() {
    assert!(report_reporting_status(true, "mainnet").is_ok());
    assert!(report_reporting_status(false, "testnet").is_ok());
    assert!(report_reporting_status(false, "dev").is_ok());
    assert!(report_reporting_status(false, "mainnet").is_err());
    assert!(report_reporting_status(false, "").is_err());
}

#[test]
fn resolve_environment_does_not_default_an_absent_var_to_a_permitted_value() {
    // Owns SENTRY_ENVIRONMENT for the duration of the test and restores
    // whatever was there before, since this is a process-global var and
    // no other test touches it.
    let previous = std::env::var("SENTRY_ENVIRONMENT").ok();

    // SAFETY: no other test reads or writes SENTRY_ENVIRONMENT.
    unsafe {
        std::env::remove_var("SENTRY_ENVIRONMENT");
    }
    let absent = resolve_environment();
    assert_ne!(
        absent, "dev",
        "an absent SENTRY_ENVIRONMENT must not resolve to a permitted value"
    );
    assert_eq!(
        reporting_status(false, &absent),
        ReportingStatus::DisabledRefused,
        "the resolved value for an absent var must fail closed, not boot disabled on mainnet"
    );

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_ENVIRONMENT", "testnet");
    }
    assert_eq!(resolve_environment(), "testnet");

    // SAFETY: see above.
    unsafe {
        match &previous {
            Some(value) => std::env::set_var("SENTRY_ENVIRONMENT", value),
            None => std::env::remove_var("SENTRY_ENVIRONMENT"),
        }
    }
}

/// Drives `build_subscriber`, the function both binaries call to build the
/// subscriber they install, rather than a layer assembled in the test. An INFO
/// record must be dropped at `ERROR`, and an INFO record with a structured
/// field must arrive as a log carrying that field once the threshold allows it.
#[test]
fn built_subscriber_applies_the_threshold_and_carries_structured_fields() {
    let build = |level| {
        // `off` for the stdout layer: only the Sentry layer is under test.
        build_subscriber(tracing_subscriber::EnvFilter::new("off"), false, level)
    };

    let dispatcher = tracing::Dispatch::new(build(tracing::Level::ERROR));
    let guard = tracing::dispatcher::set_default(&dispatcher);
    let envelopes = sentry::test::with_captured_envelopes_options(
        || tracing::info!(order = "o-1", "below the threshold"),
        client_options(None, None, "test".to_string(), 0.0),
    );
    assert!(
        captured_logs(&envelopes).is_empty(),
        "build_subscriber(ERROR) let an INFO record through as a structured log"
    );
    drop(guard);

    let dispatcher = tracing::Dispatch::new(build(tracing::Level::INFO));
    let _guard = tracing::dispatcher::set_default(&dispatcher);
    let envelopes = sentry::test::with_captured_envelopes_options(
        || tracing::info!(order = "o-1", "at the threshold"),
        client_options(None, None, "test".to_string(), 0.0),
    );
    let logs = captured_logs(&envelopes);
    assert!(
        !logs.is_empty(),
        "build_subscriber(INFO) dropped an INFO record"
    );
    assert!(
        logs.iter().all(|l| l.attributes.contains_key("order")),
        "structured field did not arrive as an attribute: {logs:?}"
    );
}

/// The binaries must get their subscriber from `build_subscriber` and never
/// assemble a Sentry layer themselves: a bare `sentry_tracing::layer()` there
/// would bypass the filter the test above covers and nothing else would notice.
#[test]
fn binaries_build_their_subscriber_through_build_subscriber() {
    let init_sites = [
        ("evmmonitor", include_str!("../bin/evmmonitor/main.rs")),
        (
            "server",
            include_str!("../../../server/src/tracing_init.rs"),
        ),
    ];
    for (name, source) in init_sites {
        assert!(
            source.contains("telemetry::build_subscriber("),
            "{name} does not install the subscriber from build_subscriber"
        );
        assert!(
            !source.contains("sentry_tracing::layer()"),
            "{name} assembles its own Sentry layer"
        );
    }
}

/// Collects what the stdout layer would write.
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> SharedBuf {
        self.clone()
    }
}

/// The scrubber used to guard only the Sentry hooks, so a secret echoed into a
/// log line reached the process log untouched. The sink the binaries write to
/// must redact too, in both output formats: phrases, key=value secrets (also as
/// quoted JSON fields) and key-shaped hex with or without 0x. Addresses stay
/// readable for operators.
#[test]
fn log_sink_redacts_credentials_but_keeps_addresses() {
    let phrase = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
    let key_hex = "ab".repeat(32);
    let addr = format!("0x{}", "cd".repeat(20));
    for json in [false, true] {
        let sink = SharedBuf::default();
        let dispatcher = tracing::Dispatch::new(build_subscriber_to(
            sink.clone(),
            tracing_subscriber::EnvFilter::new("info"),
            json,
            tracing::Level::ERROR,
        ));
        tracing::dispatcher::with_default(&dispatcher, || {
            tracing::error!(input = phrase, "rejected {phrase}");
            tracing::info!(token = "fieldsecretvalue", addr = %addr, "api_key=supersecretvalue seen");
            tracing::warn!("bad key 0x{key_hex} and bare {key_hex}");
        });
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(
            !out.contains("alpha bravo"),
            "json={json}: phrase leaked: {out}"
        );
        assert!(out.contains("[redacted-mnemonic]"), "json={json}: {out}");
        assert!(!out.contains("supersecretvalue"), "json={json}: {out}");
        assert!(!out.contains("fieldsecretvalue"), "json={json}: {out}");
        assert!(
            !out.contains(&key_hex),
            "json={json}: key hex leaked: {out}"
        );
        assert!(out.contains(&addr), "json={json}: address redacted: {out}");
    }
}
