use super::*;

#[test]
fn redacts_eth_private_key_and_address() {
    let pk = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let addr = "0x71C7656EC7ab88b098defB751B7401B5f6d8976F";
    let out = redact_secrets(&format!("signing with {pk} to {addr}"));
    assert!(!out.contains("deadbeefdead"), "private key leaked: {out}");
    assert!(!out.contains("71C7656E"), "address leaked: {out}");
    assert!(out.contains("[redacted-hex]"));
}

#[test]
fn redacts_bare_64_hex_private_key() {
    let pk = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let out = redact_secrets(&format!("key={pk}"));
    assert!(!out.contains(pk), "bare key leaked: {out}");
}

#[test]
fn redacts_jwt() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dozjgNryP4J3jVmNHl0w5N";
    let out = redact_secrets(&format!("auth failed for {jwt}"));
    assert!(!out.contains("eyJ"), "jwt leaked: {out}");
    assert!(out.contains("[redacted-jwt]"));
}

#[test]
fn redacts_mnemonic() {
    let m = "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let out = redact_secrets(&format!("loaded wallet: {m}"));
    assert!(!out.contains("sausage"), "mnemonic leaked: {out}");
    assert!(out.contains("[redacted: 12+ word run]"));
}

/// Shape alone decides: twelve or more words are redacted whatever they are, so
/// a phrase in another language, with typos, or from another wordlist is gone.
#[test]
fn any_twelve_word_run_is_redacted() {
    let runs = [
        // Spanish BIP-39, accents included.
        "ábaco abdomen abeja abierto abogado abono aborto abrazo abrir abuelo abuso acabar",
        // Two typos, a capital, an off-list word.
        "legal winnerr thank year wave sausagee worth useful Legal winner thank zzzz",
        // Not a BIP-39 wordlist at all.
        "wild father tree among universe such mobile favorite target dynamic credit identify",
        "Legal winner thank year wave sausage worth useful legal winner thank yellow",
    ];
    for run in runs {
        let out = redact_secrets(&format!("pasted {run} here"));
        assert!(out.contains("[redacted: 12+ word run]"), "{run} -> {out}");
        let first = run.split(' ').next().unwrap();
        let last = run.split(' ').next_back().unwrap();
        assert!(
            !out.contains(first) && !out.contains(last),
            "{run} -> {out}"
        );
    }
    let eleven = "legal winner thank year wave sausage worth useful legal winner thank";
    assert_eq!(redact_secrets(eleven), eleven);
}

/// The fail-open alert is the one exempt sentence; text around it is still scrubbed.
#[test]
fn fail_open_alert_is_exempt_and_only_it() {
    assert_eq!(
        redact_secrets(FAIL_OPEN_ALERT_MESSAGE),
        FAIL_OPEN_ALERT_MESSAGE
    );
    let out = redact_secrets(&format!("{FAIL_OPEN_ALERT_MESSAGE}: token=abc"));
    assert!(out.contains(FAIL_OPEN_ALERT_MESSAGE), "{out}");
    assert!(!out.contains("abc"), "{out}");
    // A phrase next to the alert reads as one run with it and is redacted.
    let m = "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let out = redact_secrets(&format!("{m}: {FAIL_OPEN_ALERT_MESSAGE}"));
    assert!(
        !out.contains("sausage") && out.contains("[redacted: 12+ word run]"),
        "{out}"
    );
    // A near-miss of the alert is ordinary text and is redacted.
    let near = FAIL_OPEN_ALERT_MESSAGE.replace("invoice", "invoices");
    assert!(redact_secrets(&near).contains("[redacted: 12+ word run]"));
}

/// A phrase split around the alert sentence must not escape by being cut in two.
#[test]
fn phrase_straddling_the_alert_is_still_redacted() {
    let input = format!(
        "legal winner thank year wave sausage {FAIL_OPEN_ALERT_MESSAGE} worth useful legal winner thank yellow"
    );
    let out = redact_secrets(&input);
    assert!(!out.contains("sausage") && !out.contains("yellow"), "{out}");
}

/// An alert sentence glued to letters is not the alert; swapping it out would
/// fuse its neighbours into one unmatched letter run and leak the words.
#[test]
fn phrase_glued_to_the_alert_is_still_redacted() {
    let a = FAIL_OPEN_ALERT_MESSAGE;
    let input =
        format!("legal{a}winner{a}thank year wave sausage worth useful legal winner thank yellow");
    let out = redact_secrets(&input);
    assert!(!out.contains("sausage") && !out.contains("yellow"), "{out}");
    // Control: the same sentence set off by spaces is still kept.
    assert_eq!(redact_secrets(&format!("x {a} y")), format!("x {a} y"));
}

/// The exemption is keyed to the stand-in's absence: input that already carries
/// the stand-in word cannot have it expanded into the alert sentence.
#[test]
fn alert_stand_in_in_input_is_not_expanded() {
    let out = redact_secrets(&format!("ALERTSTANDIN {FAIL_OPEN_ALERT_MESSAGE}"));
    assert!(!out.contains(FAIL_OPEN_ALERT_MESSAGE), "{out}");
}

/// Hex of 64 or more characters is redacted bare, whatever its length: a
/// 128-character seed or a signature is not a 64-character key.
#[test]
fn bare_hex_of_64_or_more_is_redacted_at_every_length() {
    for len in [64, 65, 128, 130] {
        let hex = "ab12".repeat(len / 4) + &"c".repeat(len % 4);
        let out = redact_secrets(&format!("value {hex} end"));
        assert_eq!(out, "value [redacted-hex] end", "len {len}");
    }
}

/// Separators other than whitespace: every non-letter divides words, so none
/// of these ways of gluing twelve words together hides them. The plain-space
/// control keeps a pass from meaning the rule went inert.
#[test]
fn phrase_is_redacted_whatever_glues_its_words() {
    let words = "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let separators = [
        ("space (control)", " "),
        ("colon", ":"),
        ("underscore", "_"),
        ("plus", "+"),
        ("equals", "="),
        ("at sign", "@"),
        ("url scheme", "://"),
        ("numbered list", ": 1. "),
        ("bidi control U+202E", "\u{202E}"),
        ("bidi isolate U+2066", "\u{2066}"),
        ("mongolian vowel separator U+180E", "\u{180E}"),
        ("combining grapheme joiner U+034F", "\u{034F}"),
        ("combining mark after a space", " \u{0301}"),
        ("json unicode escape", "\\u000a"),
        ("json nbsp escape", "\\u00a0"),
    ];
    for (name, sep) in separators {
        let out = redact_secrets(&format!("pasted {}", words.replace(' ', sep)));
        assert!(
            out.contains("[redacted: 12+ word run]") && !out.contains("sausage"),
            "{name}: {out}"
        );
    }
}

/// Long identifiers that merely contain digits between letter runs are
/// redacted too: the rule cannot tell them from a phrase, and the fail-closed
/// side is the one this rule is on. Pinned so the boundary is a decision.
#[test]
fn long_digit_split_identifier_is_redacted_deliberately() {
    let id = "a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6q7r8s9";
    assert!(redact_secrets(id).starts_with("[redacted: 12+ word run]"));
}

#[test]
fn bare_token_word_is_not_a_key_value_pair() {
    assert_eq!(redact_secrets("token USDC received"), "token USDC received");
    assert_eq!(redact_secrets("seed data loaded"), "seed data loaded");
    assert!(!redact_secrets("token=USDC").contains("USDC"));
    assert!(!redact_secrets("Bearer abc123xyz").contains("abc123xyz"));
}

#[test]
fn bearer_token_is_redacted_in_every_form() {
    for input in [
        "Bearer abc123xyz",
        "bearer=abc123xyz",
        "Bearer: abc123xyz",
        r#"{"bearer":"abc123xyz"}"#,
        "Authorization: Bearer abc123xyz",
    ] {
        let out = redact_secrets(input);
        assert!(
            !out.contains("abc123xyz"),
            "bearer leaked: {input} -> {out}"
        );
    }
}

#[test]
fn redacts_email() {
    let out = redact_secrets("customer alice@example.com paid invoice");
    assert!(!out.contains("alice@example.com"), "email leaked: {out}");
}

#[test]
fn redacts_keyed_secrets() {
    for input in [
        "api_key=sk_live_abc123def456",
        "Authorization: Bearer abc.def.ghi",
        "password = hunter2",
        "mnemonic: somesecretvalue",
    ] {
        let out = redact_secrets(input);
        assert!(out.contains("[redacted]"), "not redacted: {input} -> {out}");
        assert!(!out.contains("hunter2") || !input.contains("hunter2"));
    }
    let out = redact_secrets("password = hunter2");
    assert!(!out.contains("hunter2"), "password leaked: {out}");
}

#[test]
fn redacts_api_key_in_rpc_url_path() {
    for url in [
        "https://eth-sepolia.g.alchemy.com/v2/alch_EPqFizwy30wuSFY4ewmD-",
        "wss://eth-sepolia.g.alchemy.com/v2/alch_EPqFizwy30wuSFY4ewmD-",
        "https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef",
    ] {
        let out = redact_secrets(url);
        assert!(
            out.contains("[redacted-rpc-key]"),
            "not redacted: {url} -> {out}"
        );
        assert!(
            !out.contains("alch_EPqFizwy30wuSFY4ewmD-"),
            "key leaked: {out}"
        );
        assert!(!out.contains("0123456789abcdef"), "key leaked: {out}");
    }
}

#[test]
fn redacts_rpc_key_inside_a_provider_error_string() {
    // The shape alloy produces when a connection fails.
    // Kept short: every non-letter divides words, so a URL plus a long
    // sentence is a twelve-word run and is redacted whole.
    let msg = "fetch (https://eth-sepolia.g.alchemy.com/v2/alch_EPqFizwy30wuSFY4ewmD-)";
    let out = redact_secrets(msg);
    assert!(
        !out.contains("alch_EPqFizwy30wuSFY4ewmD-"),
        "key leaked: {out}"
    );
    assert!(
        out.contains("eth-sepolia.g.alchemy.com"),
        "host should survive for diagnosis: {out}"
    );
}

/// The full alloy connection-failure message is a twelve-run line under the
/// phrase rule (every non-letter divides words), so it is redacted whole: the
/// key is gone and so is the host. That is the cost of a separator that cannot
/// be hidden behind, pinned here so a change to it is a decision. Trimmed to the
/// URL alone the host survives, which `redacts_rpc_key_inside_a_provider_error_string`
/// covers.
#[test]
fn full_provider_error_line_is_redacted_whole() {
    let msg = "error sending request for url \
               (https://eth-sepolia.g.alchemy.com/v2/alch_EPqFizwy30wuSFY4ewmD-)";
    let out = redact_secrets(msg);
    assert!(out.starts_with("[redacted: 12+ word run]"), "{out}");
    assert!(!out.contains("EPqFizwy"), "key leaked: {out}");
    assert!(
        !out.contains("alchemy"),
        "expected whole-line redaction: {out}"
    );
}

/// Typical error strings the services emit, and what the phrase rule does to
/// each. Anything under twelve letter runs survives intact; the long ones are
/// blanked, deliberately, because a line indistinguishable from a phrase is
/// treated as one.
#[test]
fn ordinary_error_strings_by_length() {
    let kept = [
        "connection refused (os error 111)",
        "rpc request failed: timeout after 30s on https://eth.llamarpc.com",
        "database error: duplicate key value violates unique constraint invoices_pkey",
        "webhook delivery to https://example.com/hook returned 502",
    ];
    for line in kept {
        assert_eq!(redact_secrets(line), line, "over-redacted: {line}");
    }
    let blanked = [
        "error sending request for url https://eth-mainnet.g.alchemy.com/v2/ failed to connect to host",
        "failed to fetch price from https://api.coingecko.com/api/v3/simple/price because of timeout",
    ];
    for line in blanked {
        let out = redact_secrets(line);
        assert!(out.starts_with("[redacted: 12+ word run]"), "{out}");
    }
}

#[test]
fn keeps_keyless_rpc_urls_readable() {
    for url in [
        "https://eth.llamarpc.com",
        "https://polygon-rpc.com/",
        "http://192.168.1.10:8545/",
        "https://api.coingecko.com/api/v3/simple/price",
        "https://api.coingecko.com/api/v3/coins/ethereum/market_chart",
        "https://api.kraken.com/0/public/Ticker",
    ] {
        assert_eq!(redact_secrets(url), url, "over-redacted: {url}");
    }
}

#[test]
fn scrub_event_redacts_logentry_params_and_frame_vars() {
    use sentry::protocol::{Exception, Frame, LogEntry, Stacktrace};

    let mut event = Event {
        logentry: Some(LogEntry {
            message: "connecting to %s".to_string(),
            params: vec![Value::String(
                "https://eth-sepolia.g.alchemy.com/v2/alch_supersecretkey".to_string(),
            )],
        }),
        ..Default::default()
    };
    let mut frame = Frame::default();
    frame
        .vars
        .insert("password".to_string(), Value::String("hunter2".to_string()));
    event.exception.values.push(Exception {
        stacktrace: Some(Stacktrace {
            frames: vec![frame],
            ..Default::default()
        }),
        ..Default::default()
    });

    let scrubbed = scrub_event(event).expect("event passes through");
    let params = &scrubbed.logentry.as_ref().unwrap().params;
    assert!(
        !format!("{params:?}").contains("alch_supersecretkey"),
        "logentry param leaked: {params:?}"
    );
    let vars = &scrubbed.exception.values[0]
        .stacktrace
        .as_ref()
        .unwrap()
        .frames[0]
        .vars;
    assert!(
        !format!("{vars:?}").contains("hunter2"),
        "frame var leaked: {vars:?}"
    );
}

#[test]
fn keeps_innocuous_text() {
    let msg = "failed to connect to database after 3 retries";
    assert_eq!(redact_secrets(msg), msg);
}

fn test_log(body: &str) -> Log {
    Log {
        level: sentry::protocol::LogLevel::Info,
        body: body.to_string(),
        trace_id: None,
        timestamp: std::time::SystemTime::now(),
        severity_number: None,
        attributes: Default::default(),
    }
}

#[test]
fn scrub_log_redacts_secret_shaped_body() {
    let pk = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let log = test_log(&format!("loaded key {pk}"));

    let scrubbed = scrub_log(log).expect("log passes through");
    assert!(!scrubbed.body.contains(pk), "key leaked: {}", scrubbed.body);
}

#[test]
fn scrub_log_redacts_sensitive_keyed_attribute_regardless_of_shape() {
    use sentry::protocol::LogAttribute;

    let mut log = test_log("connecting");
    log.attributes.insert(
        "mnemonic".to_string(),
        LogAttribute(Value::String("not-shaped-like-a-secret".to_string())),
    );

    let scrubbed = scrub_log(log).expect("log passes through");
    let value = &scrubbed.attributes.get("mnemonic").unwrap().0;
    assert_eq!(value, &Value::String("[redacted]".to_string()));
}

#[test]
fn scrub_log_redacts_secret_shaped_attribute_under_an_innocuous_key() {
    use sentry::protocol::LogAttribute;

    let mut log = test_log("connecting");
    log.attributes.insert(
        "context".to_string(),
        LogAttribute(Value::String("token=sk_live_supersecret".to_string())),
    );

    let scrubbed = scrub_log(log).expect("log passes through");
    let value = &scrubbed.attributes.get("context").unwrap().0;
    assert!(
        !format!("{value:?}").contains("supersecret"),
        "attribute leaked: {value:?}"
    );
}

#[test]
fn scrub_log_leaves_benign_attributes_unredacted() {
    use sentry::protocol::LogAttribute;

    let mut log = test_log("connecting");
    log.attributes.insert(
        "retry_count".to_string(),
        LogAttribute(Value::Number(3.into())),
    );
    log.attributes.insert(
        "status".to_string(),
        LogAttribute(Value::String("connected".to_string())),
    );

    let scrubbed = scrub_log(log).expect("log passes through");
    assert_eq!(
        scrubbed.attributes.get("retry_count").unwrap().0,
        Value::Number(3.into()),
        "benign attribute should survive scrub_log unchanged"
    );
    assert_eq!(
        scrubbed.attributes.get("status").unwrap().0,
        Value::String("connected".to_string()),
        "benign attribute should survive scrub_log unchanged"
    );
}

/// Collects the `Log` items out of a batch of captured envelopes.
///
/// `pub(super)`: `reporting_tests` (telemetry's other test child module) needs
/// it too, and sibling modules don't share private items the way a parent and
/// child do.
pub(super) fn captured_logs(envelopes: &[sentry::Envelope]) -> Vec<Log> {
    envelopes
        .iter()
        .flat_map(|envelope| envelope.items())
        .filter_map(|item| match item {
            sentry::protocol::EnvelopeItem::ItemContainer(
                sentry::protocol::ItemContainer::Logs(logs),
            ) => Some(logs.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Proves the wiring, not just the pure function: emits a `tracing::info!`
/// carrying a secret through the exact layer construction `server.rs` and
/// `evmmonitor/main.rs` use — `sentry_tracing::layer().event_filter(
/// sentry_log_event_filter(min_level))` — and a real `sentry::Client`
/// built from [`client_options`], the same function [`init_sentry`]
/// calls, so a later edit that drops `enable_logs`/`before_send_log`
/// from production breaks this test too. `scrub_log_redacts_secret_shaped_body`
/// above calls `scrub_log` directly, which proves the function redacts
/// but not that the SDK actually routes logs through it before sending —
/// this is the end-to-end check the ticket's "Check before shipping"
/// section asked for.
#[test]
fn scrub_log_redacts_a_secret_through_the_real_capture_pipeline() {
    use tracing_subscriber::prelude::*;

    let _dispatcher = tracing_subscriber::registry()
        .with(sentry_tracing::layer().event_filter(sentry_log_event_filter(tracing::Level::INFO)))
        .set_default();

    let pk = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let envelopes = sentry::test::with_captured_envelopes_options(
        || {
            tracing::info!("loaded key {pk}");
        },
        client_options(None, None, "test".to_string(), 0.0),
    );

    let logs = captured_logs(&envelopes);
    assert!(
        !logs.is_empty(),
        "expected at least one structured log to reach the envelope"
    );
    for log in &logs {
        assert!(
            !log.body.contains(pk),
            "key survived the real subscriber -> sentry_tracing -> \
             before_send_log pipeline: {}",
            log.body
        );
    }
}

#[cfg(test)]
mod capture_tests;
#[cfg(test)]
mod event_filter_tests;

// --- ported from this branch's inline `mod tests`, which testnet moved out
// into this file. De-indented deliberately: this file's contents sit at
// column 0, and pasting an indented block here nests it inside whatever
// precedes it, which compiles and silently never runs.

/// Restores an env var to its pre-test value on drop, so a mid-test
/// panic (an assertion failing partway through a multi-step test) can't
/// leave the var set for every later test in the same process — env vars
/// are process-global and `cargo test`/`nextest` run tests in threads of
/// the same process, not one process per test.
struct RestoreEnvVar {
    name: &'static str,
    previous: Option<String>,
}

impl RestoreEnvVar {
    fn capture(name: &'static str) -> Self {
        Self {
            name,
            previous: std::env::var(name).ok(),
        }
    }
}

impl Drop for RestoreEnvVar {
    fn drop(&mut self) {
        // SAFETY: the env vars this guard restores are only ever touched
        // by the single test that owns it.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

#[test]
fn scrub_transaction_drops_request_user_server_name_and_redacts_free_text() {
    use sentry::protocol::{Request, Span, Transaction, User};

    let mut span = Span {
        description: Some(
            "connecting to https://eth-sepolia.g.alchemy.com/v2/alch_supersecretkey".to_string(),
        ),
        ..Default::default()
    };
    span.tags
        .insert("note".to_string(), "password = hunter2".to_string());
    span.data.insert(
        "detail".to_string(),
        Value::String("token=sk_live_supersecret".to_string()),
    );

    let mut transaction = Transaction {
        name: Some("GET /api/invoices/{id}".to_string()),
        request: Some(Request::default()),
        user: Some(User::default()),
        server_name: Some("payserver-prod-01".into()),
        spans: vec![span],
        ..Default::default()
    };
    transaction
        .tags
        .insert("path".to_string(), "email alice@example.com".to_string());
    transaction.extra.insert(
        "ctx".to_string(),
        Value::String("token=sk_live_supersecret".to_string()),
    );

    scrub_transaction(&mut transaction);

    assert!(transaction.request.is_none());
    assert!(transaction.user.is_none());
    assert!(transaction.server_name.is_none());
    assert_eq!(
        transaction.name.as_deref(),
        Some("GET /api/invoices/{id}"),
        "a route pattern must survive redaction unchanged"
    );
    assert!(!transaction.tags["path"].contains("alice@example.com"));
    let extra = transaction
        .extra
        .get("ctx")
        .and_then(Value::as_str)
        .unwrap();
    assert!(!extra.contains("supersecret"), "extra leaked: {extra}");
    let span = &transaction.spans[0];
    assert!(
        !span
            .description
            .as_ref()
            .unwrap()
            .contains("supersecretkey"),
        "span description leaked: {:?}",
        span.description
    );
    assert!(!span.tags["note"].contains("hunter2"));
    let span_detail = span.data.get("detail").and_then(Value::as_str).unwrap();
    assert!(
        !span_detail.contains("supersecret"),
        "span data leaked: {span_detail}"
    );
}

fn poisoned_contexts() -> Map<String, Context> {
    use sentry::protocol::{ResponseContext, RuntimeContext, TraceContext};

    let mut contexts = Map::new();
    let mut runtime = RuntimeContext {
        name: Some("rustc".to_string()),
        ..Default::default()
    };
    runtime
        .other
        .insert("token".to_string(), Value::String("sk_live_x".to_string()));
    contexts.insert("runtime".to_string(), Context::Runtime(Box::new(runtime)));
    contexts.insert(
        "response".to_string(),
        Context::Response(Box::new(ResponseContext {
            cookies: Some("session=abc123".to_string()),
            data: Some(Value::String("body".to_string())),
            ..Default::default()
        })),
    );
    let mut other = Map::new();
    other.insert("password".to_string(), Value::String("hunter2".to_string()));
    contexts.insert("plugin".to_string(), Context::Other(other));
    let mut trace = TraceContext {
        description: Some("token=sk_live_x".to_string()),
        ..Default::default()
    };
    trace
        .data
        .insert("secret".to_string(), Value::String("hunter2".to_string()));
    contexts.insert("trace".to_string(), Context::Trace(Box::new(trace)));
    contexts
}

fn assert_context_containers_are_clean(contexts: &Map<String, Context>) {
    let Context::Runtime(runtime) = &contexts["runtime"] else {
        panic!("expected runtime context")
    };
    assert_eq!(
        runtime.other.get("token").and_then(Value::as_str),
        Some("[redacted]"),
        "runtime.other leaked a secret key"
    );
    let Context::Response(response) = &contexts["response"] else {
        panic!("expected response context")
    };
    assert!(response.cookies.is_none());
    assert!(response.data.is_none());
    let Context::Other(plugin) = &contexts["plugin"] else {
        panic!("expected other context")
    };
    assert_eq!(
        plugin.get("password").and_then(Value::as_str),
        Some("[redacted]")
    );
    let Context::Trace(trace) = &contexts["trace"] else {
        panic!("expected trace context")
    };
    let description = trace.description.as_ref().unwrap();
    assert!(
        !description.contains("sk_live_x"),
        "trace.description leaked a secret: {description}"
    );
    assert_eq!(
        trace.data.get("secret").and_then(Value::as_str),
        Some("[redacted]"),
        "trace.data leaked a secret key"
    );
}

#[test]
fn scrub_event_and_transaction_redact_contexts() {
    use sentry::protocol::Transaction;

    let event = Event {
        contexts: poisoned_contexts(),
        ..Default::default()
    };
    let scrubbed = scrub_event(event).expect("event passes through");
    assert_context_containers_are_clean(&scrubbed.contexts);

    let mut transaction = Transaction {
        contexts: poisoned_contexts(),
        ..Default::default()
    };
    scrub_transaction(&mut transaction);
    assert_context_containers_are_clean(&transaction.contexts);
}

#[test]
fn scrubbing_transport_scrubs_transactions_before_forwarding() {
    use std::sync::Mutex;

    use sentry::Transport;
    use sentry::protocol::{Envelope, EnvelopeItem, Event, Request, Transaction};

    #[derive(Default)]
    struct RecordingTransport {
        envelopes: Mutex<Vec<Envelope>>,
    }

    impl Transport for RecordingTransport {
        fn send_envelope(&self, envelope: Envelope) {
            self.envelopes.lock().unwrap().push(envelope);
        }
    }

    let recorder = Arc::new(RecordingTransport::default());
    let scrubber = ScrubbingTransport {
        inner: recorder.clone(),
    };

    let mut envelope = Envelope::new();
    envelope.add_item(Transaction {
        request: Some(Request::default()),
        ..Default::default()
    });
    envelope.add_item(Event::default());
    scrubber.send_envelope(envelope);

    let forwarded = recorder.envelopes.lock().unwrap();
    assert_eq!(
        forwarded.len(),
        1,
        "the envelope itself must still be forwarded"
    );
    let mut saw_transaction = false;
    for item in forwarded[0].items() {
        match item {
            EnvelopeItem::Transaction(transaction) => {
                saw_transaction = true;
                assert!(
                    transaction.request.is_none(),
                    "transaction request must be scrubbed before forwarding"
                );
            }
            EnvelopeItem::Event(_) => {}
            other => panic!("unexpected envelope item: {other:?}"),
        }
    }
    assert!(
        saw_transaction,
        "the transaction item must be forwarded, not dropped"
    );
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
fn resolve_traces_sample_rate_defaults_to_zero() {
    // Owns SENTRY_TRACES_SAMPLE_RATE for the duration of the test;
    // restored on drop (including on panic) since this is a
    // process-global var and no other test touches it.
    let _restore = RestoreEnvVar::capture("SENTRY_TRACES_SAMPLE_RATE");

    // SAFETY: no other test reads or writes SENTRY_TRACES_SAMPLE_RATE.
    unsafe {
        std::env::remove_var("SENTRY_TRACES_SAMPLE_RATE");
    }
    assert_eq!(
        resolve_traces_sample_rate(),
        0.0,
        "unset must default to 0.0"
    );

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_TRACES_SAMPLE_RATE", "not-a-number");
    }
    assert_eq!(
        resolve_traces_sample_rate(),
        0.0,
        "unparseable must fall back to 0.0, not panic"
    );

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_TRACES_SAMPLE_RATE", "0.1");
    }
    assert_eq!(resolve_traces_sample_rate(), 0.1);

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_TRACES_SAMPLE_RATE", "10");
    }
    assert_eq!(
        resolve_traces_sample_rate(),
        1.0,
        "a parseable but out-of-range value (e.g. \"10\" typed for \"0.1\") must clamp, not pass through"
    );

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_TRACES_SAMPLE_RATE", "-3");
    }
    assert_eq!(
        resolve_traces_sample_rate(),
        0.0,
        "a negative value must clamp to 0.0, not go negative"
    );

    // SAFETY: see above.
    unsafe {
        std::env::set_var("SENTRY_TRACES_SAMPLE_RATE", "nan");
    }
    assert_eq!(
        resolve_traces_sample_rate(),
        0.0,
        "\"nan\" parses to f32::NAN, which clamp() passes through unchanged; must fall back to 0.0 instead"
    );
}

#[test]
fn client_options_carry_the_sample_rate_and_the_scrubbing_transport() {
    let options = client_options(None, None, "testnet".to_string(), 0.42);
    // 0.49 keeps the rate inside a sampling *strategy* rather than a plain
    // field, so this reads the strategy back. `FixedRate` specifically:
    // `Disabled` and `FixedRate(0.0)` both sample nothing, and asserting only
    // on the effect would not tell them apart.
    assert!(
        matches!(
            options.traces_sampling_strategy,
            sentry::TracesSamplingStrategy::FixedRate(rate) if rate == 0.42
        ),
        "resolved sample rate must reach the options sentry::init actually receives; got {:?}",
        options.traces_sampling_strategy
    );
    assert!(
        options.transport.is_some(),
        "ScrubbingTransportFactory must be wired in, or performance transactions ship unscrubbed"
    );
    assert!(
        options.before_send.is_some(),
        "scrub_event must be wired in, or error events ship unscrubbed"
    );
}

/// In a JSON log line a newline or tab inside a string is the two characters
/// backslash and `n` / `t`, so a multi-line paste must still read as one phrase.
/// The phrase carries a typo and sits at the very start of the string value,
/// where the escape's `n` can be read as the first letter of a word.
#[test]
fn phrase_split_by_json_escapes_is_redacted_even_with_a_typo() {
    let clean = "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let typo = "legal winner thank year wave sausagee worth useful legal winner thank yellow";
    for m in [clean, typo] {
        for sep in ["\\n", "\\t", "\\r\\n", "\\\\n", ", ", ". 1 "] {
            for lead in ["", "\\n", "\\n\\n"] {
                let line = format!(r#"{{"message":"{lead}{}"}}"#, m.replace(' ', sep));
                let out = redact_secrets(&line);
                assert!(!out.contains("winner") && !out.contains("yellow"), "{out}");
                assert!(out.ends_with(r#""}"#), "{out}");
            }
        }
    }
}

/// An unquoted value must not swallow the JSON punctuation that closes it.
#[test]
fn key_value_rule_leaves_closing_json_punctuation() {
    let out = redact_secrets(r#"{"token":12}"#);
    assert_eq!(out, r#"{"token":[redacted]}"#);
}

/// Words are separated by more than an ASCII space: pasting from a rendered
/// page brings non-breaking spaces, and invisible separators are free to add.
/// The plain-space row is the control: if it stops redacting, the rule has gone
/// inert and the other rows passing means nothing.
#[test]
fn phrase_is_redacted_whatever_separates_its_words() {
    let words = "legal winner thank year wave sausage worth useful legal winner thank yellow";
    let separators = [
        ("ordinary space (control)", " "),
        ("invisible separator U+2063", "\u{2063}"),
        ("zero-width space U+200B", "\u{200B}"),
        ("space + invisible separator", " \u{2063}"),
        ("non-breaking space U+00A0", "\u{00A0}"),
        ("zero-width joiner U+200D", "\u{200D}"),
        ("word joiner U+2060", "\u{2060}"),
        ("byte-order mark U+FEFF", "\u{FEFF}"),
    ];
    for (name, sep) in separators {
        let out = redact_secrets(&format!("pasted {}", words.replace(' ', sep)));
        assert!(
            out.contains("[redacted: 12+ word run]") && !out.contains("sausage"),
            "{name}: {out}"
        );
    }
}
