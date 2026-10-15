//! The container-log path, driven through the real subscriber stack.
//!
//! These capture what `build_subscriber_with_writer` (the function the binaries'
//! `build_subscriber` delegates to) actually writes, rather than testing the
//! redacting writer in isolation: a writer nothing installs proves nothing.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::captured_logs;
use crate::telemetry::*;

const PHRASE: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

fn run(json: bool) -> (String, Vec<sentry::protocol::Log>) {
    let sink = Sink::default();
    let subscriber = build_subscriber_with_writer(
        tracing_subscriber::EnvFilter::new("info"),
        json,
        tracing::Level::INFO,
        sink.clone(),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    let envelopes = sentry::test::with_captured_envelopes_options(
        || {
            tracing::info!("loaded wallet: {PHRASE}");
            tracing::info!(note = %PHRASE, "field form");
        },
        client_options(None, None, "test".to_string(), 0.0),
    );
    let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    (out, captured_logs(&envelopes))
}

#[test]
fn a_mnemonic_logged_at_info_reaches_stdout_redacted_and_sentry_still_redacted() {
    for json in [false, true] {
        let (out, logs) = run(json);
        assert!(
            out.contains("loaded wallet"),
            "nothing reached stdout: {out}"
        );
        assert!(out.contains("field form"), "nothing reached stdout: {out}");
        assert!(
            !out.contains("sausage"),
            "mnemonic leaked to stdout (json={json}): {out}"
        );
        assert!(out.contains("[redacted-mnemonic]"), "{out}");
        assert!(!logs.is_empty(), "nothing reached Sentry");
        assert!(
            !format!("{logs:?}").contains("sausage"),
            "mnemonic leaked to Sentry: {logs:?}"
        );
    }
}

#[test]
fn a_line_that_is_not_utf8_is_still_redacted_not_passed_through() {
    let sink = Sink::default();
    let mw = stdout_redact::RedactingMakeWriter::new(sink.clone());
    {
        use tracing_subscriber::fmt::MakeWriter;
        let mut w = mw.make_writer();
        // Secret split across writes, with an invalid byte in the middle.
        w.write_all(b"bad \xff byte; legal winner thank year wave ")
            .unwrap();
        w.write_all(b"sausage worth useful legal winner thank yellow\n")
            .unwrap();
    }
    let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(!out.contains("sausage"), "{out}");
    assert!(out.contains('\u{FFFD}'), "{out}");
}
