//! The fail-open alert must reach error reporting intact. Its text is a long
//! plain sentence; the telemetry scrubber must not mistake it for a recovery
//! phrase, or the alert arrives as a bare placeholder and says nothing.

use super::*;
use tracing_subscriber::prelude::*;

/// The alert text exactly as `surface_fail_open_allow` emits it. Deliberately a
/// second literal, not `FAIL_OPEN_ALERT_MESSAGE`: if the emitter's wording is
/// ever changed without the scrubber's exemption, these tests go red instead of
/// the alert quietly turning into a placeholder. Do not tidy it into the constant.
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

#[derive(Clone, Default)]
struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

/// Through the real emitting function and the real stdout layer, in both log
/// formats: the line the container log carries still says what the alert says.
#[test]
fn emitted_fail_open_alert_survives_the_stdout_sink() {
    for json in [false, true] {
        let sink = Buf::default();
        let dispatcher = tracing::Dispatch::new(evm::telemetry::build_subscriber_to(
            sink.clone(),
            tracing_subscriber::EnvFilter::new("info"),
            json,
            tracing::Level::ERROR,
        ));
        tracing::dispatcher::with_default(&dispatcher, || {
            surface_fail_open_allow(
                &PluginId::new("cash.random.billing").unwrap(),
                "acct",
                Some(&serde_json::json!({ "basis": "never_received" })),
                chrono::Utc::now(),
            );
        });
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains(FAIL_OPEN_MESSAGE), "json={json}: {out}");
        assert!(!out.contains("redacted"), "json={json}: {out}");
    }
}
