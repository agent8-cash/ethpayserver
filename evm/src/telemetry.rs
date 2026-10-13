//! Error-telemetry scrubbing for a Sentry-protocol error telemetry backend.
//!
//! random.cash is a crypto payment processor, so error payloads must **never**
//! carry secrets or customer data: wallet/private keys, mnemonics, API keys,
//! JWTs, bearer tokens, emails, on-chain addresses/hashes, HTTP request bodies,
//! or per-user identity. [`scrub_event`] is installed as the Sentry
//! `before_send` hook in every binary that initialises Sentry, [`scrub_log`]
//! as `before_send_log` for the separate structured-logs pipeline, and
//! [`redact_secrets`] redacts secret-shaped substrings from free-text fields.
//!
//! This lives in `evm` (rather than being duplicated per binary) so the
//! `server` and `evmmonitor` binaries share one audited implementation. It is
//! gated behind the `sentry-scrub` feature so the scrubber and its `sentry` /
//! `regex` dependencies are only compiled where Sentry is actually used.
//!
//! # Relationship to the `scrub` crate
//!
//! The same rules exist a second time in payserver-commons `scrub`, as
//! hand-written scanners with no dependencies, because the browser client
//! needs them and `regex` costs ~1 MB of WASM. This regex table stays the
//! audited reference, and `parity_with_shared_scrubber` below asserts the two
//! agree across a corpus — so changing the rules here fails the build until
//! `scrub` follows. That check lives here because this is the only crate
//! where both implementations compile: `evm` pulls in alloy/sqlx and does not
//! build for `wasm32`, which is the whole reason there are two.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::OnceLock;

use regex::Regex;
use sentry::protocol::{Context, Event, Log, Map, Value};

/// Placeholder for a run of twelve or more words. It names what was matched,
/// not what was inferred: the matcher cannot know a seed phrase was present,
/// only that the text has the shape of one.
const PHRASE_PLACEHOLDER: &str = "[redacted: 12+ word run]";

/// The one sentence exempt from the word-run rule: the fail-open alert, which
/// is fixed, finite and long enough to look like a phrase. Everything else of
/// that shape is redacted whatever its words are, so a phrase in any language,
/// with any typos, or from a non-BIP-39 wordlist, never survives. Emitters must
/// use this constant so the exemption and the alert cannot drift apart.
pub const FAIL_OPEN_ALERT_MESSAGE: &str = "invoice creation allowed on a standing that was never received or is older than the freshness bound";

/// Stand-in for the exempt alert while rules run; a plain word so it counts
/// towards a surrounding run.
const ALERT_STAND_IN: &str = "ALERTSTANDIN";

/// Ordered `(pattern, replacement)` redaction rules applied to every free-text
/// field. Compiled once and reused for the life of the process.
fn rules() -> &'static [(Regex, &'static str)] {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RULES.get_or_init(|| {
        // `unwrap` is safe: these are constant, test-covered patterns.
        #[allow(clippy::unwrap_used)]
        let build = |p: &str| Regex::new(p).unwrap();
        vec![
            // JSON Web Tokens (header.payload.signature).
            (
                build(r"eyJ[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+"),
                "[redacted-jwt]",
            ),
            // RPC provider URLs carry the API key in the path (Alchemy
            // `/v2/<key>`, Infura `/v3/<key>`, QuickNode `/<token>/`), which no
            // `key=value` rule can see. Redact any path segment long enough to
            // be a credential and keep scheme/host, so reports still say which
            // provider failed. Query-string forms (`?api-key=`) are already
            // covered by the `key: value` rule below. The 16-char floor clears
            // real path words (`market_chart`, `getting-started`) while still
            // catching short provider keys — checked against live Alchemy,
            // Infura, QuickNode, CoinGecko, Etherscan and Kraken URL shapes.
            (
                build(
                    r"((?:https?|wss?)://[A-Za-z0-9.\-]+(?::[0-9]+)?(?:/[A-Za-z0-9._\-]{1,15})*/)[A-Za-z0-9._\-]{16,}",
                ),
                "${1}[redacted-rpc-key]",
            ),
            // 0x-prefixed hex of address length or longer: addresses (40),
            // private keys / tx hashes / block hashes (64), signatures (130).
            (build(r"0x[0-9a-fA-F]{40,}"), "[redacted-hex]"),
            // Bare hex of 64 or more characters (the 0x-prefixed form is the
            // rule above) is the shape of a private key, a transaction hash and
            // a signature alike; nothing in the text tells them apart, so the
            // error report loses hashes rather than risk a key.
            (build(r"\b[0-9a-fA-F]{64,}\b"), "[redacted-hex]"),
            // Recovery phrases: 12 or more runs of letters, of any case, script
            // or wordlist, divided by separators. Membership is deliberately not
            // checked: a wordlist would let other languages, typos and
            // non-BIP-39 seeds through, and a destroyed line costs less than a
            // leaked seed. A separator is whitespace of every kind, any control,
            // format (zero-width, bidi, invisible) or combining character, a JSON
            // escape of a line break or of a code point, list and quote
            // punctuation, or a list number set off from the words
            // (`1. legal 2. winner`, `1) legal`), which is how wallets print a
            // backup. Digits glued to letters are not (`a1b2c3...`).
            (
                build(
                    &r#"(?:\p{L}+(?:\\+u[0-9a-fA-F]{4}|\\+[nrt]|SEP\d{1,3}[.):\-]*|\d{1,3}[.):\-]*SEP|SEP)+){11,}\p{L}+"#
                        .replace("SEP", r#"[\s\p{Cc}\p{Cf}\p{M},;|()\[\]{}"'\\]"#),
                ),
                PHRASE_PLACEHOLDER,
            ),
            // The same run joined by one repeated punctuation character and
            // nothing else (`legal-winner-thank-...`, `legal_winner_...`,
            // `legal.winner...`). Mixed punctuation and digits stay out, so
            // paths, URLs, hex and `key:value` JSON read as before; twelve words
            // joined by the same character is not a path.
            (
                build(
                    &["-", "_", ".", ":", "/", "+", "="]
                        .map(|c| format!(r"(?:\p{{L}}+{}){{11,}}\p{{L}}+", regex::escape(c)))
                        .join("|"),
                ),
                PHRASE_PLACEHOLDER,
            ),
            // Email addresses (customer PII).
            (
                build(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"),
                "[redacted-email]",
            ),
            // `Bearer <tok>`: the scheme word is only a credential marker when a
            // token follows it.
            (
                build(r#"(?i)\bbearer\s+[^\s,;"'}]+"#),
                "Bearer [redacted]",
            ),
            // The words that name a credential and nothing else may take their
            // value after plain whitespace (`private-key <hex>`, `password
            // hunter2`). Ahead of the pair rule, and refusing a value that opens
            // with a separator, so `API_KEY = x` is left to the pair rule.
            (
                build(
                    r#"(?i)\b(api[_-]?key|password|passwd|private[_-]?key|authorization)\b(\s+)[^\s,;"'}\\:=][^\s,;"'}\\]*"#,
                ),
                "$1$2[redacted]",
            ),
            // `key: value` / `key=value` for sensitive keys. A JSON line escapes
            // a quoted value (`token: \"abc\"`), so the opening quote is
            // optionally escaped and the value stops at a backslash that opens an
            // escape. A `}` or a backslash followed by more value belongs to it
            // (`abc}def`), so the tail of a secret is not left behind; a `}` that
            // ends the value does not. A bare word with only whitespace after it
            // is not a pair for these: `token USDC received` is ordinary
            // vocabulary here, so a separator is required.
            (
                build(
                    r#"(?i)\b(api[_-]?key|secret|password|passwd|token|bearer|mnemonic|seed|private[_-]?key|authorization)\b(\\*"?\s*[:=]\s*)(\\*"?)(?:[^\s,;"'}\\]|\\[^nrt"'\\]|\}[^\s,;"'}\\])+"#,
                ),
                "$1$2$3[redacted]",
            ),
        ]
    })
}

/// Redact secret-shaped substrings from a free-text string.
///
/// Defence-in-depth for any error/log text that may have interpolated a
/// secret. Patterns intentionally err on the side of over-redaction.
#[must_use]
pub fn redact_secrets(input: &str) -> String {
    // The exempt alert sentence is swapped for a single stand-in word, so the
    // matcher never sees it and never has to infer anything about it. A word
    // rather than a cut keeps a phrase that straddles the alert one contiguous
    // run: splitting the input around it would leave two halves under twelve
    // words each, and the exemption would become a way to smuggle a phrase past
    // the rule. Whatever the rules redact takes the stand-in with it.
    // Input already holding the stand-in could forge the alert on the way out.
    // Only an occurrence with a non-letter (or the string edge) on both sides is
    // exempt: glued to letters it is not the alert, and swapping it for the
    // stand-in would fuse the neighbours into one letter run no rule matches.
    // An unexempt occurrence stays in the text and is redacted as the run of
    // words it is.
    if input.contains(FAIL_OPEN_ALERT_MESSAGE) && !input.contains(ALERT_STAND_IN) {
        let mut masked = String::with_capacity(input.len());
        let mut last = 0;
        for (start, m) in input.match_indices(FAIL_OPEN_ALERT_MESSAGE) {
            let end = start + m.len();
            let before = input[..start].chars().next_back();
            let after = input[end..].chars().next();
            if start >= last
                && !before.is_some_and(char::is_alphabetic)
                && !after.is_some_and(char::is_alphabetic)
            {
                masked.push_str(&input[last..start]);
                masked.push_str(ALERT_STAND_IN);
                last = end;
            }
        }
        if last > 0 {
            masked.push_str(&input[last..]);
            return redact_secrets(&masked).replace(ALERT_STAND_IN, FAIL_OPEN_ALERT_MESSAGE);
        }
    }
    let mut out = std::borrow::Cow::Borrowed(input);
    for (re, replacement) in rules() {
        if re.is_match(&out) {
            out = std::borrow::Cow::Owned(re.replace_all(&out, *replacement).into_owned());
        }
    }
    out.into_owned()
}

/// Recursively redact secrets inside a JSON value (used for `extra` / breadcrumb
/// `data` blobs).
fn redact_value(value: &mut Value) {
    match value {
        Value::String(s) => *s = redact_secrets(s),
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        Value::Object(map) => redact_map(map.iter_mut()),
        _ => {}
    }
}

/// Redact a structured-data map, keyed.
///
/// The text rules need key and value in one string (`password = hunter2`). In a
/// map they are separate, so a bare "hunter2" matches nothing — the key is the
/// only signal there is. Every map on an event goes through here (`extra`,
/// breadcrumb data, stack-frame vars), which is exactly where that shape shows
/// up.
fn redact_map<'a>(entries: impl IntoIterator<Item = (&'a String, &'a mut Value)>) {
    for (key, value) in entries {
        if is_sensitive_key(key) {
            *value = Value::String("[redacted]".to_string());
        } else {
            redact_value(value);
        }
    }
}

/// Whether a structured-data key names something whose value is a secret.
fn is_sensitive_key(key: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "api_key",
        "api-key",
        "mnemonic",
        "seed",
        "privatekey",
        "private_key",
        "private-key",
        "authorization",
        "auth",
        "dsn",
        "credential",
        "session",
        "cookie",
    ];
    let key = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|needle| key.contains(needle))
}

/// Redact the open-ended parts of a `contexts` map (present on both
/// [`Event`] and `Transaction`) — every typed variant (`os`, `runtime`,
/// `device`, ...) carries an `other: Map<String, Value>` catch-all for
/// forward compatibility, `Context::Other` is fully free-form,
/// `Context::Response` carries HTTP response cookies/headers/body, and
/// `Context::Trace` carries a free-text `description` plus its own open
/// `data` map. Nothing in this codebase calls `Scope::set_context` today, so
/// these are populated only by Sentry's own integrations: `contexts`
/// (OS/runtime/device introspection — fixed, not secret-shaped fields, so
/// left alone here) and `sentry-tower` (attaches `Context::Trace` to every
/// transaction, which *is* closed below since it carries free text and an
/// open map like the others). Any context variant not named above is dropped
/// rather than forwarded — see the wildcard arm below.
fn redact_contexts(contexts: &mut Map<String, Context>) {
    // `Context` is `#[non_exhaustive]` upstream, so this match can never be
    // exhaustive over its variants and a future SDK bump can add one we've
    // never seen. The wildcard arm can't reach into an unrecognised
    // variant's fields to redact them — it can only see that the variant
    // exists — so rather than pass it through unscrubbed, drop it and log
    // which type name got dropped.
    contexts.retain(|_, context| match context {
        Context::Other(map) => {
            redact_map(map.iter_mut());
            true
        }
        Context::Device(c) => {
            redact_map(c.other.iter_mut());
            true
        }
        Context::Os(c) => {
            redact_map(c.other.iter_mut());
            true
        }
        Context::Runtime(c) => {
            redact_map(c.other.iter_mut());
            true
        }
        Context::App(c) => {
            redact_map(c.other.iter_mut());
            true
        }
        Context::Browser(c) => {
            redact_map(c.other.iter_mut());
            true
        }
        Context::Gpu(c) => {
            redact_map(c.other.iter_mut());
            true
        }
        Context::Otel(c) => {
            redact_map(c.attributes.iter_mut());
            redact_map(c.resource.iter_mut());
            redact_map(c.other.iter_mut());
            true
        }
        Context::Response(c) => {
            // Same reasoning as `event.request`/`transaction.request`:
            // an HTTP response container routinely holds cookies/headers.
            c.cookies = None;
            c.headers.clear();
            c.data = None;
            true
        }
        Context::Trace(c) => {
            // `sentry-tower` attaches one of these to every transaction,
            // so once tracing is on this ships on essentially every
            // envelope. `description` is free text and `data` is an open
            // catch-all, same shape as the other variants above.
            if let Some(description) = c.description.as_mut() {
                *description = redact_secrets(description);
            }
            redact_map(c.data.iter_mut());
            true
        }
        other => {
            tracing::warn!(
                context_type = %other.type_name(),
                "dropping Sentry context of a type with no redaction rule"
            );
            false
        }
    });
}

/// Sentry `before_send` hook: strip PII/secrets before an event leaves the
/// process. Returning `Some(event)` lets the (scrubbed) event through;
/// returning `None` would drop it entirely.
///
/// Drops whole high-risk containers (HTTP request, user identity, server name)
/// and redacts secret-shaped text from every remaining free-text field.
#[must_use]
pub fn scrub_event(mut event: Event<'static>) -> Option<Event<'static>> {
    // Drop entire containers that routinely hold secrets / PII. With
    // `send_default_pii = false` these are usually empty, but never assume.
    event.request = None; // HTTP method/url/headers/cookies/body
    event.user = None; // id / email / ip / username
    event.server_name = None; // host identity
    redact_contexts(&mut event.contexts);

    // Redact secret-shaped text from remaining free-text fields.
    for text in [
        &mut event.message,
        &mut event.culprit,
        &mut event.transaction,
        &mut event.logger,
    ]
    .into_iter()
    .flatten()
    {
        *text = redact_secrets(text);
    }
    if let Some(logentry) = event.logentry.as_mut() {
        logentry.message = redact_secrets(&logentry.message);
        // `message` is the template ("connecting to %s"); the values live in
        // `params`, so scrubbing only the message leaves the secret shipping.
        logentry.params.iter_mut().for_each(redact_value);
    }
    for exception in &mut event.exception.values {
        if let Some(value) = exception.value.as_mut() {
            *value = redact_secrets(value);
        }
        // Frame-local variables are captured verbatim where a backtrace
        // integration fills them in.
        if let Some(stacktrace) = exception.stacktrace.as_mut() {
            for frame in &mut stacktrace.frames {
                redact_map(frame.vars.iter_mut());
            }
        }
    }
    for breadcrumb in &mut event.breadcrumbs.values {
        if let Some(message) = breadcrumb.message.as_mut() {
            *message = redact_secrets(message);
        }
        redact_map(breadcrumb.data.iter_mut());
    }
    redact_map(event.extra.iter_mut());
    for tag_value in event.tags.values_mut() {
        *tag_value = redact_secrets(tag_value);
    }

    Some(event)
}

/// Applies [`scrub_event`]'s policy to a performance transaction.
///
/// `ClientOptions::before_send` (and `Scope`'s event processors) only run for
/// error events: a `Transaction` is built, filled in by the `sentry-tower`
/// integration — including the request URL and headers, via
/// `TransactionOrSpan::set_request` — and handed straight to the transport in
/// `Span::finish`, with no callback in between. Turning on
/// `traces_sample_rate` therefore opens a second, unscrubbed path off the
/// host unless something scrubs the transaction itself; [`ScrubbingTransport`]
/// calls this just before an envelope is sent.
fn scrub_transaction(transaction: &mut sentry::protocol::Transaction<'static>) {
    // Same containers scrub_event drops: the request the tower integration
    // attaches carries the raw URL and headers, not the route pattern.
    transaction.request = None;
    transaction.user = None;
    transaction.server_name = None;
    redact_contexts(&mut transaction.contexts);

    if let Some(name) = transaction.name.as_mut() {
        *name = redact_secrets(name);
    }
    for tag_value in transaction.tags.values_mut() {
        *tag_value = redact_secrets(tag_value);
    }
    redact_map(transaction.extra.iter_mut());

    for span in &mut transaction.spans {
        if let Some(description) = span.description.as_mut() {
            *description = redact_secrets(description);
        }
        for tag_value in span.tags.values_mut() {
            *tag_value = redact_secrets(tag_value);
        }
        redact_map(span.data.iter_mut());
    }
}

/// Wraps the real transport so every outgoing envelope's `Transaction` items
/// pass through [`scrub_transaction`] first. This is the only seam available
/// for that in this SDK version — see [`scrub_transaction`] for why
/// `before_send` doesn't reach transactions.
struct ScrubbingTransport {
    inner: Arc<dyn sentry::Transport>,
}

impl sentry::Transport for ScrubbingTransport {
    fn send_envelope(&self, envelope: sentry::protocol::Envelope) {
        let mut scrubbed =
            sentry::protocol::Envelope::new().with_headers(envelope.headers().clone());
        for item in envelope.into_items() {
            match item {
                sentry::protocol::EnvelopeItem::Transaction(mut transaction) => {
                    scrub_transaction(&mut transaction);
                    // 0.49 holds the transaction boxed inside the variant and
                    // no longer converts a `Box<Transaction>` on its own, so
                    // re-wrap explicitly rather than leaning on `Into`.
                    scrubbed.add_item(sentry::protocol::EnvelopeItem::Transaction(transaction));
                }
                other => scrubbed.add_item(other),
            }
        }
        self.inner.send_envelope(scrubbed);
    }

    fn flush(&self, timeout: std::time::Duration) -> bool {
        self.inner.flush(timeout)
    }

    fn shutdown(&self, timeout: std::time::Duration) -> bool {
        self.inner.shutdown(timeout)
    }
}

/// Builds the real (reqwest) transport and wraps it in [`ScrubbingTransport`].
/// Installed as `ClientOptions::transport` in [`init_sentry`] instead of
/// leaving it `None`, which would fall back to the same reqwest transport
/// unscrubbed.
struct ScrubbingTransportFactory;

impl sentry::TransportFactory for ScrubbingTransportFactory {
    // `create_transport_with_options`, not the older `create_transport`: as of
    // 0.49 this is the method the SDK actually calls, and the older one is
    // documented as not called at all. A factory that implements only the old
    // one still works today, through a default that round-trips the options
    // back into a `ClientOptions` - but it is reached by a deprecated
    // compatibility shim, and the day that shim goes, the scrubber goes with
    // it and every performance transaction ships unscrubbed with nothing red.
    fn create_transport_with_options(
        &self,
        options: sentry::TransportOptions,
    ) -> Arc<dyn sentry::Transport> {
        Arc::new(ScrubbingTransport {
            inner: Arc::new(sentry::transports::ReqwestHttpTransportOptions::from(options).build()),
        })
    }
}

/// Sentry `before_send_log` hook: strip PII/secrets before a structured log
/// record leaves the process.
///
/// Structured logs are a separate Sentry pipeline from events/breadcrumbs —
/// `before_send` above never sees them — so without this hook a log line
/// that interpolated a mnemonic, a JWT, or an on-chain address would ship
/// unredacted. Mirrors [`scrub_event`]: `body` is the free-text message,
/// `attributes` is the structured-data map, redacted the same way `extra` is.
#[must_use]
pub fn scrub_log(mut log: Log) -> Option<Log> {
    log.body = redact_secrets(&log.body);
    for (key, attribute) in log.attributes.iter_mut() {
        if is_sensitive_key(key) {
            attribute.0 = Value::String("[redacted]".to_string());
        } else {
            redact_value(&mut attribute.0);
        }
    }
    Some(log)
}

/// Whether error reporting is on, and — when it is off — whether that is
/// acceptable for the environment reporting failed to catch this itself once:
/// a disabled integration looks identical to a working one unless something
/// says so at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportingStatus {
    /// A DSN is configured.
    Enabled,
    /// No DSN, but that's permitted in this (known-safe) environment.
    DisabledPermitted,
    /// No DSN in an environment that must not boot like this: `mainnet`,
    /// or anything unrecognised. An unset/unknown environment is the exact
    /// shape the incident that motivated this took — nothing said it was
    /// wrong — so it fails closed rather than being allowlisted as safe.
    DisabledRefused,
}

/// Decide whether error reporting is enabled and, if not, whether that's
/// permitted. Only `testnet` and `dev` are permitted to run disabled;
/// `mainnet` *and every other value, including unset*, are refused. Fails
/// closed: a typo'd or missing `SENTRY_ENVIRONMENT` must not be silently
/// treated as safe the way a missing `SENTRY_DSN` was.
///
/// Pure and side-effect free: the caller is responsible for logging the
/// result and, for [`ReportingStatus::DisabledRefused`], for refusing to
/// continue booting.
#[must_use]
pub fn reporting_status(dsn_configured: bool, environment: &str) -> ReportingStatus {
    if dsn_configured {
        ReportingStatus::Enabled
    } else if matches!(environment, "testnet" | "dev") {
        ReportingStatus::DisabledPermitted
    } else {
        ReportingStatus::DisabledRefused
    }
}

/// Resolve the `SENTRY_ENVIRONMENT` tag. A single source of truth so
/// [`init_sentry`] (what gets sent to Sentry) and
/// [`report_reporting_status`] (what a human sees at boot) cannot default it
/// two different ways and drift apart.
///
/// An unset variable resolves to the empty string, **not** to a permitted
/// value like `"dev"`. `reporting_status` already treats `""` the same as any
/// other unrecognised environment (refused). Defaulting it to `"dev"` here
/// used to silently launder "nobody set this" into "known safe to run
/// disabled" before `reporting_status` ever saw it — the exact failure shape
/// this ticket exists to close, one layer up.
#[must_use]
pub fn resolve_environment() -> String {
    std::env::var("SENTRY_ENVIRONMENT").unwrap_or_default()
}

/// Resolve `SENTRY_TRACES_SAMPLE_RATE`: the fraction of requests sampled for
/// performance tracing, from `0.0` (none) to `1.0` (all). Defaults to `0.0`
/// — no transactions leave the process — so tracing stays off until an
/// environment opts in. An unset value falls back to `0.0` silently (that's
/// the expected "not configured" state); an unparseable one also falls back
/// to `0.0` rather than failing boot over it, since sending no transactions
/// is always a safe default, but logs a warning first — otherwise a typo'd
/// value is indistinguishable from an intentional `0.0` and can sit
/// unnoticed indefinitely. A parseable but out-of-range value (e.g. `"1"`
/// typed for `"0.1"`, or a negative number) is clamped into `0.0..=1.0` with
/// the same warning, rather than handed to `ClientOptions` as-is — otherwise
/// that exact typo silently reproduces the unbounded-cost failure this
/// ticket exists to close. `NaN` parses successfully (`"nan"` is a valid
/// `f32`) but compares `false` against every bound, so it would skip the
/// range check and `f32::clamp` passes it through unchanged — it is treated
/// as unparseable rather than trusted to `clamp`.
#[must_use]
pub fn resolve_traces_sample_rate() -> f32 {
    match std::env::var("SENTRY_TRACES_SAMPLE_RATE") {
        Ok(raw) => {
            let parsed: f32 = raw
                .parse()
                .ok()
                .filter(|value: &f32| !value.is_nan())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        value = %raw,
                        "SENTRY_TRACES_SAMPLE_RATE is not a valid number; falling back to 0.0"
                    );
                    0.0
                });
            if (0.0..=1.0).contains(&parsed) {
                parsed
            } else {
                tracing::warn!(
                    value = %raw,
                    "SENTRY_TRACES_SAMPLE_RATE is outside 0.0..=1.0; clamping"
                );
                parsed.clamp(0.0, 1.0)
            }
        }
        Err(_) => 0.0,
    }
}

/// Builds the `ClientOptions` [`init_sentry`] hands to `sentry::init`. Split
/// out so a test can construct the exact options production uses — via
/// `sentry::test::with_captured_envelopes_options` — instead of hand-rolling
/// a lookalike `ClientOptions` that could quietly drift from what actually
/// ships.
fn client_options(
    dsn: Option<sentry::types::Dsn>,
    release: Option<Cow<'static, str>>,
    environment: String,
    traces_sample_rate: f32,
) -> sentry::ClientOptions {
    // `ClientOptions` is `#[non_exhaustive]` as of 0.49, which makes the old
    // struct-literal (`..Default::default()` included) construction illegal
    // from outside the crate. Built via the builder chain plus direct field
    // mutation instead - both still allowed on an already-constructed value.
    let mut options = sentry::ClientOptions::new()
        .maybe_release(release)
        .environment(environment)
        // Never attach default PII (IP, cookies, request bodies). This is a
        // payment processor.
        .send_default_pii(false)
        // 0.49 made this a builder method that *panics* outside 0.0..=1.0,
        // so the clamping in `resolve_traces_sample_rate` is what keeps a
        // malformed env var from taking the process down at startup rather
        // than merely being tidy. Note also that 0.49 treats an explicit
        // `0.0` as a fixed rate of zero, which is distinct from leaving
        // sampling unset - same effect, nothing sampled, but it is a
        // deliberate "off", not a default.
        .traces_sample_rate(traces_sample_rate)
        // Mandatory secret/PII scrubber: redacts wallet keys, mnemonics, JWTs,
        // API keys, emails and on-chain addresses before events leave the host.
        .before_send(scrub_event)
        // Structured logs (see `sentry_log_event_filter` for which levels
        // actually reach this). Same mandatory scrubber, via the separate
        // hook logs go through.
        .before_send_log(scrub_log);
    options.dsn = dsn;
    // `before_send` does not run for performance transactions in this SDK
    // version (see `scrub_transaction`), so the transport scrubs them
    // instead - otherwise the mandatory scrubber is only half applied once
    // `traces_sample_rate` can be nonzero.
    options.transport = Some(Arc::new(ScrubbingTransportFactory));
    // `enable_logs` is deprecated as of 0.49: "logs captured manually are
    // always sent; only automatic capture by integrations respects this
    // option". `sentry_tracing::layer()` is exactly such an integration - it
    // is what gets a `tracing` event into an envelope here - so the note says
    // the option still governs us, not that it is vestigial.
    //
    // That is established by ablation, not read off a changelog. On 0.47,
    // `disabling_enable_logs_suppresses_automatic_integration_capture` in
    // `capture_tests` takes this exact `client_options()` output, flips only
    // this field to `false`, and asserts no structured log reaches the
    // envelope. The same ablation was then run against a real 0.49.3 build:
    // with `enable_logs: true` all four `capture_tests` pass exactly as on
    // 0.47, and flipping only this field to `false` reproduces the identical
    // three "expected at least one structured log to reach the envelope"
    // failures. That result was recorded on this file's own comment while the
    // pin was still 0.47, by the change that established it; the recipe is
    // above and can be re-run against whatever version the next bump proposes.
    //
    // This is the pull request that moves the pin, so this is where the
    // `#[allow(deprecated)]` the ablation called for belongs. Dropping the
    // field, or silencing the lint as though it no longer mattered, would
    // turn off structured-log capture on the PII path.
    #[allow(deprecated)]
    {
        options.enable_logs = true;
    }
    options
}

/// Returns the init guard, whether a DSN was actually configured, and the
/// resolved environment tag — pass the latter two to
/// [`report_reporting_status`].
pub fn init_sentry(release: Option<Cow<'static, str>>) -> (sentry::ClientInitGuard, bool, String) {
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .and_then(|s| s.parse().ok());
    let dsn_configured = dsn.is_some();
    let environment = resolve_environment();
    let guard = sentry::init(client_options(
        dsn,
        release,
        environment.clone(),
        resolve_traces_sample_rate(),
    ));
    (guard, dsn_configured, environment)
}

/// Env var controlling which tracing levels become Sentry *structured logs*,
/// independently of `LOG_LEVEL` (which controls what the process emits at
/// all, e.g. to stdout/Loki). Unset or unparsable resolves to `WARN`, the
/// stricter option, so a deploy that forgets to set this does not start
/// billing/shipping `mainnet` INFO logs to a third party by default; testnet
/// sets this to `info` explicitly to get the noisier feed.
#[must_use]
pub fn resolve_sentry_log_level() -> tracing::Level {
    match std::env::var("SENTRY_LOG_LEVEL") {
        Err(_) => tracing::Level::WARN,
        Ok(value) => value.parse().unwrap_or_else(|_| {
            // Unlike an unset var, this is a misconfiguration: someone set
            // the knob and got it wrong, so the fallback to WARN should be
            // visible rather than indistinguishable from "correctly set to
            // WARN".
            tracing::warn!(
                value = %value,
                "SENTRY_LOG_LEVEL is set but not a valid tracing level; defaulting to WARN"
            );
            tracing::Level::WARN
        }),
    }
}

/// Drops the `Log` flag from `filter` when `event_level` is more verbose than
/// `min_level`. Split out from [`sentry_log_event_filter`] so the threshold
/// logic is testable without constructing a real `tracing::Metadata`.
fn apply_log_level_gate(
    filter: sentry_tracing::EventFilter,
    event_level: tracing::Level,
    min_level: tracing::Level,
) -> sentry_tracing::EventFilter {
    if event_level > min_level {
        filter.difference(sentry_tracing::EventFilter::Log)
    } else {
        filter
    }
}

/// Wraps [`sentry_event_filter`] (which already downgrades
/// `alloy_transport_ws` noise to a breadcrumb), additionally dropping the
/// `Log` flag for any record more verbose than `min_level` — the knob behind
/// [`resolve_sentry_log_level`]. Breadcrumbs and error events are untouched:
/// this only changes whether a record also becomes a Sentry structured log.
pub fn sentry_log_event_filter(
    min_level: tracing::Level,
) -> impl Fn(&tracing::Metadata<'_>) -> sentry_tracing::EventFilter + Send + Sync + 'static {
    move |metadata| {
        apply_log_level_gate(
            sentry_tracing::default_event_filter(metadata),
            *metadata.level(),
            min_level,
        )
    }
}

/// Sentry event filter for the `sentry_tracing` layer installed by the
/// `server` and `evmmonitor` binaries.
///
/// `alloy_transport_ws` logs at `error!` for every ordinary WebSocket hiccup a
/// long-lived RPC connection sees - a proxy resetting an idle socket, a
/// missed keepalive pong - and `sentry_tracing`'s default filter turns any
/// `error!` into a full Sentry event regardless of which crate logged it. So
/// every blip the library logs was paging as if nothing were handling it.
/// Built on top of [`sentry_log_event_filter`] rather than
/// `default_event_filter` directly, so the WS-target demotion and the
/// `SENTRY_LOG_LEVEL` gate compose through one filter instead of the two
/// binaries needing to install two separate `event_filter` layers.
///
/// This filter only ever needs to swallow an *isolated* blip, never a
/// persistent failure, because it is not the backstop for a connection that
/// stays down: `ChainMonitor::resubscribe_if_stalled`
/// (`evm/src/monitor/chain/lifecycle.rs`) already watches for that on its own
/// clock, independent of anything `alloy_transport_ws` or `alloy_pubsub` logs
/// or doesn't log. It resubscribes and logs its own `error!` under
/// `evm::monitor::chain::lifecycle` - a target this filter never touches -
/// once a block stream has gone silent for `stall_timeout`. So a transient
/// reset stays a breadcrumb, and a connection that never recovers pages
/// within one stall window regardless of what the WS layer's own retry logic
/// happens to be doing underneath it. `our_own_errors_still_page` covers that
/// target generically, and `evm/tests/stalled_stream_still_pages.rs` drives a
/// real stall through a real `ChainMonitor` to confirm
/// `resubscribe_if_stalled`'s own `error!` resolves to a paging event, not
/// just a hand-typed target string.
///
/// An earlier version of this filter argued instead that `alloy_pubsub`'s own
/// service loop (`alloy_pubsub::service`) always logs a paging `error!` when
/// *it* gives up retrying, and used that as the backstop. That turned out not
/// to hold in general: `evm/tests/ws_pubsub_retry_escalation.rs` runs the
/// real `alloy_pubsub`/`alloy_transport_ws` retry loop (pinned to `alloy =
/// "1.0"`, resolved in `Cargo.lock` to 1.8.3) against a WS server that
/// completes the handshake and then resets every connection, including
/// retries, and `alloy_pubsub::service` never logs at all - it just
/// reconnects, dies, and reconnects again, forever. Traced against that
/// pinned source: `reconnect_with_retries`
/// (`alloy-pubsub-1.8.3/src/service.rs:195`) only counts a `reconnect()` call
/// as a failed attempt if establishing the connection itself errors; a
/// connection that establishes fine and then dies immediately after counts as
/// a *successful* reconnect, so `max_retries` is never approached and the
/// give-up log at `service.rs:205` is never reached. That is a real gap in
/// `alloy_pubsub`, not a defect in this filter - it just means this filter
/// cannot lean on it, which is why the actual backstop is our own
/// `resubscribe_if_stalled` instead.
///
/// `alloy_pubsub_giving_up_still_pages` documents the narrower case where
/// `alloy_pubsub`'s give-up log does still apply - the connection attempt
/// itself fails outright (DNS, refused, TLS) rather than flapping - which
/// still pages correctly since this filter never touches that target either.
/// It is not relied on as the general backstop.
///
/// Only `error!`-level `alloy_transport_ws` events are demoted: the noise
/// this exists to quiet is specifically the `error!` call sites in
/// `alloy_transport_ws::native`, not `debug!`/`trace!` chatter the same
/// target might log, so this filter does not touch those.
///
/// `server::services::webhook::merchant_delivery_failed` is demoted the same
/// way, for an unrelated reason: it's logged only when a webhook job
/// exhausts every retry because the request never reached the merchant's
/// endpoint at all (`WebhookError::Unreachable` - DNS, refused, or timed
/// out) *and* the caller has already checked that failure isn't isolated to
/// payserver's own egress (see
/// `server::services::webhook::service::log_permanent_failure` and
/// `recent_unreachable_are_one_merchant` for that check - a DNS/refused/
/// timeout failure alone can't tell "one merchant is down" from "we can't
/// reach anyone", so this target is only ever chosen once the caller has
/// ruled the latter out). The demoted case is already fully captured by the
/// `webhook_delivery_status="permanent_failed"` metric and the
/// `webhook_deliveries` table row the same call site writes. A non-success
/// response, a payload that failed to serialize, or an `Unreachable` failure
/// that isn't isolated to one store webhook keeps the module's default
/// target instead (see
/// `server::services::webhook::service::permanent_failure_is_merchant_unreachable`), since
/// each of those can reflect a fault in our own signing, request
/// construction, or network egress just as easily as one in the merchant's
/// server, and those must keep paging the same as `log_process_error`'s
/// "Error processing webhook job". Paging on-call for the genuinely-isolated
/// case teaches the same lesson as the WS noise above — ignore Sentry errors
/// — for a condition no payserver engineer can act on.
pub fn sentry_event_filter(
    min_level: tracing::Level,
) -> impl Fn(&tracing::Metadata<'_>) -> sentry_tracing::EventFilter + Send + Sync + 'static {
    let log_gate = sentry_log_event_filter(min_level);
    move |metadata| {
        let filter = log_gate(metadata);
        let demote = *metadata.level() == tracing::Level::ERROR
            && (metadata.target() == "alloy_transport_ws"
                || metadata.target().starts_with("alloy_transport_ws::")
                || metadata.target() == "server::services::webhook::merchant_delivery_failed");
        if demote {
            (filter - sentry_tracing::EventFilter::Event) | sentry_tracing::EventFilter::Breadcrumb
        } else {
            filter
        }
    }
}

/// The Sentry layer both binaries install: [`sentry_event_filter`] at
/// `min_level`, per-layer-filtered to INFO and above.
///
/// Per-layer filtering (rather than a shared `.with(filter)`) keeps
/// `SENTRY_LOG_LEVEL` independent of `LOG_LEVEL`: a bare filter layer ANDs
/// across the whole stack, so an event `LOG_LEVEL` rejects would never reach
/// this layer at all. The INFO floor is fixed because `sentry_tracing` never
/// does anything below INFO regardless of `min_level`.
///
/// One constructor, so `server`, `evmmonitor` and the tests build the same
/// layer: a test that assembles its own can prove the layer works, never that
/// a binary installs it.
pub fn sentry_layer<S>(min_level: tracing::Level) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::Layer;
    sentry_tracing::layer()
        .event_filter(sentry_event_filter(min_level))
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO)
}

/// The whole subscriber both binaries install: the Sentry layer from
/// [`sentry_layer`] plus a stdout `fmt` layer (JSON when `json`) filtered by
/// `log_filter`.
///
/// Each layer carries its own filter so `SENTRY_LOG_LEVEL` and `LOG_LEVEL`
/// stay independent. Returned rather than installed so a test can run the
/// exact stack a binary runs; the binaries only call `.init()` on it.
pub fn build_subscriber(
    log_filter: tracing_subscriber::EnvFilter,
    json: bool,
    sentry_min_level: tracing::Level,
) -> Box<dyn tracing::Subscriber + Send + Sync> {
    use tracing_subscriber::{Layer, layer::SubscriberExt};
    let registry = tracing_subscriber::registry().with(sentry_layer(sentry_min_level));
    if json {
        Box::new(
            registry.with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_filter(log_filter),
            ),
        )
    } else {
        Box::new(registry.with(tracing_subscriber::fmt::layer().with_filter(log_filter)))
    }
}

/// Log whether error reporting is on, at INFO, always — never the DSN itself
/// — and refuse to continue when [`reporting_status`] says this environment
/// must not run disabled.
pub fn report_reporting_status(dsn_configured: bool, environment: &str) -> anyhow::Result<()> {
    let environment = if environment.is_empty() {
        "(unset)"
    } else {
        environment
    };
    match reporting_status(dsn_configured, environment) {
        ReportingStatus::Enabled => {
            tracing::info!(environment = %environment, "error reporting enabled");
        }
        ReportingStatus::DisabledPermitted => {
            tracing::info!(
                environment = %environment,
                "error reporting DISABLED (no DSN configured); permitted in this environment"
            );
        }
        ReportingStatus::DisabledRefused => {
            anyhow::bail!(
                "error reporting DISABLED (no DSN configured) in environment={environment}; refusing to start"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod alloy_filter_tests;
#[cfg(test)]
mod reporting_tests;
#[cfg(test)]
mod tests;
