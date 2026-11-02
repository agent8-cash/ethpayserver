//! Proves `ScrubbingTransportFactory` (installed by [`client_options`], the
//! production options builder) is the transport actually handling an
//! envelope — not merely that *some* transport is configured
//! (`client_options_carry_the_sample_rate_and_the_scrubbing_transport` in the
//! parent module only asserts `Option::is_some`) and not a hand-built
//! `ScrubbingTransport`
//! (`scrubbing_transport_scrubs_transactions_before_forwarding` constructs
//! one directly, proving the wrapper scrubs but not that production code
//! ever reaches it).
//!
//! Sentry 0.49 calls `TransportFactory::create_transport_with_options`
//! directly; the older `create_transport`'s default implementation
//! round-trips back into it. Both dispatch paths therefore end at the same
//! code today, which is exactly why no in-process assertion can tell a
//! factory that only implements the old method apart from one that
//! implements the new one — see the comment beside
//! `ScrubbingTransportFactory` itself. The only way to exercise *this*
//! transport the way production wires it is to make it put bytes on a
//! socket and read them back out.
//!
//! A grandchild of `telemetry`, like its `capture_tests`/`event_filter_tests`
//! siblings: `telemetry.rs` is over the line limit and already declares
//! `mod tests`; `tests.rs` is where room is.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use crate::telemetry::client_options;

/// Accepts exactly one HTTP request on an OS-assigned loopback port, and
/// hands back a channel that yields that request's raw body once it has
/// fully arrived. `127.0.0.1:0`: deterministic, no external network, no
/// fixed port to collide with another test or CI run.
fn start_envelope_listener() -> (u16, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local listener");
    let port = listener.local_addr().expect("local addr").port();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        // Bounded, not unbounded: a wiring bug that never sends anything
        // must fail this test, not hang the suite.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

        let mut received = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
            if let Some(header_end) = find_header_end(&received) {
                let declared = parse_content_length(&received[..header_end]).unwrap_or(0);
                let body_so_far = received.len() - (header_end + 4);
                if body_so_far >= declared {
                    break;
                }
            }
        }

        let _ =
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");

        if let Some(header_end) = find_header_end(&received) {
            let _ = tx.send(received[header_end + 4..].to_vec());
        }
    });

    (port, rx)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(headers).lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

/// Builds the client the way [`crate::telemetry::init_sentry`] does —
/// `sentry::init` over [`client_options`]'s output, not a hand-rolled
/// `Client`/`Hub` lookalike, so an ablation falls back to whatever
/// `sentry::init`'s own `apply_defaults` would actually install in
/// production (the SDK's default transport) rather than to a silent no-op
/// this test's own setup invented. Points the DSN at the listener above,
/// sends a transaction carrying a secret-shaped tag value, and asserts that
/// value is gone from the bytes that actually left the process.
///
/// `sentry::init` binds the client to `Hub::current()` for the calling
/// thread only, and the default test harness runs each `#[test]` fn on its
/// own OS thread, so this does not leak into other tests running in the same
/// process.
///
/// Ablated to confirm this can fail: with `client_options`'s
/// `options.transport = Some(Arc::new(ScrubbingTransportFactory))` line
/// commented out, this test fails with
/// `the secret tag value left the process unscrubbed`. Restored before
/// committing. Reinstate that ablation to re-run this after the
/// `create_transport` compatibility shim this test guards against is
/// actually removed upstream.
#[test]
fn production_options_scrub_a_transaction_before_it_leaves_the_process() {
    let (port, body_rx) = start_envelope_listener();
    let dsn = format!("http://public@127.0.0.1:{port}/1")
        .parse()
        .expect("local dsn parses");

    const SENTINEL_FRAGMENT: &str = "scrubtest-sentinel-not-a-real-secret";
    let secret_tag = format!("api_key={SENTINEL_FRAGMENT}");

    let options = client_options(Some(dsn), None, "test".to_string(), 1.0);
    let guard = sentry::init(options);

    let ctx = sentry::TransactionContext::new("scrub-test", "test.op");
    let transaction = sentry::start_transaction(ctx);
    transaction.set_tag("leak", &secret_tag);
    transaction.finish();

    assert!(
        guard.close(Some(Duration::from_secs(5))),
        "transport did not drain its queued envelope before the timeout"
    );

    let body = body_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the transaction envelope must actually reach the listener");
    let body_text = String::from_utf8_lossy(&body);
    assert!(
        !body_text.contains(SENTINEL_FRAGMENT),
        "the secret tag value left the process unscrubbed: {body_text}"
    );
}
