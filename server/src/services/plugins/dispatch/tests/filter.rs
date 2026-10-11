//! `invoice_creation_filters` and `PluginInvoiceCreationFilter` - the export
//! consulted before an invoice is created.

use super::*;

/// The hazard the kind check exists for. An action plugin that fails
/// closed, registered as a filter, refuses every invoice on the instance.
#[test]
fn only_filters_are_registered_as_filters() {
    let host = host();
    host.register(
        manifest("cash.random.anaction", "action", None),
        &trapping(PAYMENT_SETTLED),
    )
    .unwrap();
    host.register(
        manifest("cash.random.afilter", "filter", None),
        &trapping(FILTER_INVOICE_CREATION),
    )
    .unwrap();

    let loaded = vec![
        PluginId::new("cash.random.anaction").unwrap(),
        PluginId::new("cash.random.afilter").unwrap(),
    ];

    assert_eq!(
        invoice_creation_filters(&host, &loaded).len(),
        1,
        "an action plugin must not be registered as a filter"
    );
}

/// A plugin id the host has never seen must not become a filter. Without
/// the `is_some_and`, an unknown id would fall through to a default.
#[test]
fn an_unregistered_plugin_is_not_a_filter() {
    let host = host();
    let loaded = vec![PluginId::new("cash.random.ghost").unwrap()];

    assert!(invoice_creation_filters(&host, &loaded).is_empty());
}

/// A filter whose manifest fails closed and whose call cannot run must
/// refuse - but with a message the merchant can read, not the trap text.
#[tokio::test]
async fn a_failed_closed_filter_refuses_without_leaking_internals() {
    let host = host();
    host.register(
        manifest("cash.random.strict", "filter", None),
        &trapping(FILTER_INVOICE_CREATION),
    )
    .unwrap();

    let filter = PluginInvoiceCreationFilter::new(
        Arc::clone(&host),
        PluginId::new("cash.random.strict").unwrap(),
    );

    let verdict = filter.filter_invoice_creation(a_store()).await;

    match verdict {
        FilterVerdict::Deny { reason } => {
            assert_eq!(reason, UNAVAILABLE);
            assert!(
                !reason.contains("export") && !reason.contains("wasm"),
                "a merchant must not be shown our internals: {reason}"
            );
        }
        FilterVerdict::Allow => panic!("a closed filter that could not run must refuse"),
    }
}

/// The same failure on a filter that declared `failure_mode = "open"`
/// allows instead. Billing declares open for exactly this reason: our
/// outage must not become every merchant's outage.
#[tokio::test]
async fn a_failed_open_filter_allows() {
    let host = host();
    host.register(
        manifest("cash.random.lenient", "filter", Some("open")),
        &trapping(FILTER_INVOICE_CREATION),
    )
    .unwrap();

    let filter = PluginInvoiceCreationFilter::new(
        Arc::clone(&host),
        PluginId::new("cash.random.lenient").unwrap(),
    );

    let verdict = filter.filter_invoice_creation(a_store()).await;

    assert_eq!(verdict, FilterVerdict::Allow);
}

/// The wire contract in the direction a real plugin uses it: the plugin
/// answers, and its own words reach the merchant unchanged.
///
/// The refusal message is the plugin's one chance to say what to do about
/// it ("your subscription lapsed"), so replacing it with our own generic
/// text would make every billing refusal unactionable. Every other filter
/// test here exercises a path where the plugin never answered at all.
#[tokio::test]
async fn a_plugins_refusal_reaches_the_merchant_verbatim() {
    let host = host();
    host.register(
        manifest("cash.random.answers", "filter", None),
        &answering(
            FILTER_INVOICE_CREATION,
            r#"{"allow":false,"reason":"Your subscription lapsed on 1 September."}"#,
        ),
    )
    .unwrap();

    let filter = PluginInvoiceCreationFilter::new(
        Arc::clone(&host),
        PluginId::new("cash.random.answers").unwrap(),
    );

    match filter.filter_invoice_creation(a_store()).await {
        FilterVerdict::Deny { reason } => {
            assert_eq!(reason, "Your subscription lapsed on 1 September.");
        }
        FilterVerdict::Allow => panic!("the plugin refused; the host allowed"),
    }
}

/// The same path, answering the other way. Without this, a filter stuck on
/// "deny" would pass every other test in this module.
#[tokio::test]
async fn a_plugin_that_allows_is_not_overridden() {
    let host = host();
    host.register(
        manifest("cash.random.permits", "filter", None),
        &answering(FILTER_INVOICE_CREATION, r#"{"allow":true}"#),
    )
    .unwrap();

    let filter = PluginInvoiceCreationFilter::new(
        Arc::clone(&host),
        PluginId::new("cash.random.permits").unwrap(),
    );

    assert_eq!(
        filter.filter_invoice_creation(a_store()).await,
        FilterVerdict::Allow
    );
}

#[derive(Clone, Default)]
struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
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

/// Runs one real plugin answer through the dispatch site and reports what
/// reached an operator: error-level log lines, and the fail-open counter by
/// reason.
async fn surfaced_by(answer: &str) -> (FilterVerdict, String, String) {
    surfaced_by_plugin("cash.random.billing", answer).await
}

async fn surfaced_by_plugin(id: &str, answer: &str) -> (FilterVerdict, String, String) {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    let _log = tracing::subscriber::set_default(subscriber);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let host = host();
    host.register(
        manifest(id, "filter", None),
        &answering(FILTER_INVOICE_CREATION, answer),
    )
    .unwrap();
    let filter = PluginInvoiceCreationFilter::new(Arc::clone(&host), PluginId::new(id).unwrap());
    let verdict = filter.filter_invoice_creation(a_store()).await;

    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let counters: String = handle
        .render()
        .lines()
        .filter(|l| l.starts_with(FAIL_OPEN_ALLOW_COUNTER))
        .collect::<Vec<_>>()
        .join("\n");
    (verdict, logged, counters)
}

fn heard_ago(days: i64) -> String {
    let t = chrono::Utc::now() - chrono::Duration::days(days);
    format!(
        r#"{{"allow":true,"standing_basis":{{"basis":"confirmed","last_heard_at":"{}"}}}}"#,
        t.to_rfc3339()
    )
}

#[tokio::test]
async fn an_allow_on_a_standing_never_received_is_surfaced() {
    let (verdict, logged, counters) =
        surfaced_by(r#"{"allow":true,"standing_basis":{"basis":"never_received"}}"#).await;
    assert_eq!(verdict, FilterVerdict::Allow);
    assert!(logged.contains("ERROR"), "no error event: {logged}");
    assert!(counters.contains(r#"reason="unheard"} 1"#), "{counters}");
}

#[tokio::test]
async fn an_allow_on_a_standing_older_than_the_bound_is_surfaced() {
    let days = data_service::DEFAULT_STANDING_MAX_AGE_DAYS + 1;
    let (verdict, logged, counters) = surfaced_by(&heard_ago(days)).await;
    assert_eq!(verdict, FilterVerdict::Allow);
    assert!(logged.contains("ERROR"), "no error event: {logged}");
    assert!(counters.contains(r#"reason="stale"} 1"#), "{counters}");
}

/// A recently confirmed standing raises nothing.
#[tokio::test]
async fn a_recently_confirmed_allow_is_silent() {
    let (verdict, logged, counters) = surfaced_by(&heard_ago(1)).await;
    assert_eq!(verdict, FilterVerdict::Allow);
    assert!(logged.is_empty(), "{logged}");
    assert!(counters.is_empty(), "{counters}");
}

/// The plugin that always reports a basis, answering without one, is counted
/// rather than read as healthy: otherwise it dropping the field would make
/// every fail-open invisible.
#[tokio::test]
async fn an_allow_with_no_basis_from_the_reporting_plugin_is_surfaced() {
    let (verdict, logged, counters) = surfaced_by(r#"{"allow":true}"#).await;
    assert_eq!(verdict, FilterVerdict::Allow);
    assert!(logged.contains("ERROR"), "no error event: {logged}");
    assert!(counters.contains(r#"reason="absent"} 1"#), "{counters}");
}

/// A plugin not expected to report a basis says nothing about standing, so
/// its allow must not inflate the counter.
#[tokio::test]
async fn an_allow_with_no_basis_from_another_plugin_is_silent() {
    let (verdict, logged, counters) =
        surfaced_by_plugin("cash.random.permits", r#"{"allow":true}"#).await;
    assert_eq!(verdict, FilterVerdict::Allow);
    assert!(logged.is_empty(), "{logged}");
    assert!(counters.is_empty(), "{counters}");
}

/// A deny is not a fail-open allow, whatever the basis it carries.
#[tokio::test]
async fn a_deny_is_silent_even_on_a_never_received_basis() {
    let (verdict, logged, counters) = surfaced_by(
        r#"{"allow":false,"reason":"over","standing_basis":{"basis":"never_received"}}"#,
    )
    .await;
    assert!(matches!(verdict, FilterVerdict::Deny { .. }));
    assert!(logged.is_empty(), "{logged}");
    assert!(counters.is_empty(), "{counters}");
}

/// A basis this host does not know, or a timestamp that is not RFC 3339, is
/// filed as unreadable.
///
/// Both of these now fail at one place - deserialising the shared type - and
/// "unreadable" is the honest answer for both, because each really is a
/// payload the two sides do not agree on. That was not true while the
/// timestamp was a `String`: such a payload deserialised cleanly and failed a
/// second parse further in, so a sender that had merely sent a bad value was
/// reported as one speaking a dialect. A sender holding the shared type can
/// no longer produce either of these, so reaching this arm means a sender on
/// a different pin, or something hand-rolling the JSON.
///
/// It is not the drift detector: the literals here are typed on this side.
/// Drift is caught by `what_the_shared_type_serialises_is_never_unreadable`,
/// which builds the verdict from the type the sender serialises.
#[tokio::test]
async fn an_unknown_basis_or_bad_timestamp_is_unreadable() {
    for answer in [
        r#"{"allow":true,"standing_basis":{"basis":"never_heard"}}"#,
        r#"{"allow":true,"standing_basis":{"basis":"confirmed","last_heard_at":"yesterday"}}"#,
    ] {
        let (verdict, _, counters) = surfaced_by(answer).await;
        assert_eq!(verdict, FilterVerdict::Allow);
        assert!(counters.contains(r#"reason="unreadable"} 1"#), "{counters}");
    }
}

/// The sender and this host share one `StandingBasis`; whatever it serialises
/// to must be read, never filed as unreadable. Renaming a variant or its wire
/// form in the shared type moves both sides together, and a sender still on
/// an old pin fails to compile instead of being silently misfiled.
#[tokio::test]
async fn what_the_shared_type_serialises_is_never_unreadable() {
    use payserver_plugin_api::StandingBasis;
    let stale = chrono::Utc::now()
        - chrono::Duration::days(data_service::DEFAULT_STANDING_MAX_AGE_DAYS + 1);
    for (basis, reason) in [
        (StandingBasis::NeverReceived, "unheard"),
        (
            StandingBasis::Confirmed {
                last_heard_at: stale,
            },
            "stale",
        ),
    ] {
        let answer = serde_json::json!({"allow": true, "standing_basis": basis}).to_string();
        let (_, _, counters) = surfaced_by(&answer).await;
        assert!(
            counters.contains(&format!(r#"reason="{reason}"}} 1"#)),
            "{counters}"
        );
    }
}

/// A standing confirmed within the freshness bound is not a fail-open allow:
/// the shared type's fresh form must be read, not filed as unreadable.
#[tokio::test]
async fn a_fresh_shared_confirmed_basis_raises_nothing() {
    use payserver_plugin_api::StandingBasis;
    let basis = StandingBasis::Confirmed {
        last_heard_at: chrono::Utc::now(),
    };
    let answer = serde_json::json!({"allow": true, "standing_basis": basis}).to_string();
    let (verdict, logged, counters) = surfaced_by(&answer).await;
    assert_eq!(verdict, FilterVerdict::Allow);
    assert!(logged.is_empty(), "{logged}");
    assert!(counters.is_empty(), "{counters}");
}
