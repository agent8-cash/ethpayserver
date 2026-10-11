//! The fail-open alert must reach error reporting intact. Its text is a long
//! plain sentence; the telemetry scrubber must not mistake it for a recovery
//! phrase, or the alert arrives as a bare placeholder and says nothing.

use super::*;
use tracing_subscriber::prelude::*;

/// The alert text exactly as `surface_fail_open_allow` emits it.
const FAIL_OPEN_MESSAGE: &str = "invoice creation allowed on a standing that was never received or is older than the freshness bound";

#[test]
fn fail_open_message_survives_the_telemetry_scrubber() {
    assert_eq!(
        evm::telemetry::redact_secrets(FAIL_OPEN_MESSAGE),
        FAIL_OPEN_MESSAGE
    );
}

/// Through the real emitting function and the real Sentry layer, then the
/// same scrubber the binaries install: the event's message is still the alert.
#[test]
fn emitted_fail_open_alert_keeps_its_message_after_scrubbing() {
    let _dispatcher = tracing_subscriber::registry()
        .with(evm::telemetry::sentry_layer(tracing::Level::ERROR))
        .set_default();

    let events = sentry::test::with_captured_events(|| {
        surface_fail_open_allow(
            &PluginId::new("cash.random.billing").unwrap(),
            "acct",
            Some(&serde_json::json!({ "basis": "never_received" })),
            chrono::Utc::now(),
        );
    });

    assert_eq!(events.len(), 1, "the alert should reach Sentry once");
    let scrubbed = evm::telemetry::scrub_event(events.into_iter().next().unwrap()).unwrap();
    let message = scrubbed
        .message
        .or_else(|| scrubbed.logentry.map(|l| l.message))
        .unwrap();
    assert_eq!(message, FAIL_OPEN_MESSAGE);
}
