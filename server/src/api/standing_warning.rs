//! Response-header middleware for the `X-Account-Warning` header.
//!
//! A merchant who only calls the API reads neither a dashboard nor an inbox,
//! but reads every response. When the held standing says their paid period
//! ends within [`WARN_WINDOW_DAYS`], the response says so, with the date and
//! where to renew. Outside the window nothing is added, so the header is a
//! signal rather than wallpaper.
//!
//! A header rather than a body field: no response schema changes, so no
//! existing client can break on it.
//!
//! Only the period-ending condition is carried. The volume-approaching-cap
//! condition is deferred: the pushed standing holds no volume figures or
//! warning threshold, so there is nothing here to compare against.
//!
//! Three properties are deliberate:
//!
//! - **No query per request.** The standing is read at most once per account
//!   per [`CACHE_TTL`]; every other request in that interval is answered from
//!   memory. A change therefore shows up on responses within the TTL.
//! - **It never issues anything.** Only the held standing is read. This path
//!   cannot create an invoice or call the plugin that decides standing.
//! - **It is silent about missing information.** An account with no held
//!   standing, or a held one that is not in good standing, gets no warning
//!   here: the refusal at invoice creation already says everything, and a
//!   fail-open alert per response would be noise.
//!
//! The authenticating extractor cannot set response headers, so it hands the
//! account to this middleware through a shared slot, the same bridge
//! `api_key_deprecation` uses.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use axum::{
    extract::{Request, State},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Duration, Utc};
use data_service::{AccountStandingStore, HeldStanding};
use uuid::Uuid;

/// How close to the end of the paid period the warning starts.
pub const WARN_WINDOW_DAYS: i64 = 5;

/// How long after the period ended the "ended" wording keeps appearing. Past
/// this the account is a long-lapsed one and a warning on every response would
/// be wallpaper.
pub const ENDED_NOTICE_DAYS: i64 = 7;

/// How long one account's answer is reused before the standing is read again.
pub const CACHE_TTL: StdDuration = StdDuration::from_secs(60);

/// Entries kept before the cache is emptied, so it cannot grow without bound.
const MAX_CACHE_ENTRIES: usize = 10_000;

pub static X_ACCOUNT_WARNING: HeaderName = HeaderName::from_static("x-account-warning");

/// The route, carried separately from the prose. Present only when the entitlement
/// supplied one, so its ABSENCE on a warned response is the machine-readable fact
/// that the merchant was told their period is ending and given nowhere to go.
///
/// THE ABSENCE IS LOAD-BEARING. DO NOT REPLACE IT WITH AN EXPLICIT `none` VALUE.
///
/// An absent header cannot be distinguished from one stripped by a proxy or never
/// emitted by an older build. That ambiguity would normally be a defect; here it is
/// the point. Both readings - "no route was supplied" and "cannot tell" - resolve to
/// the same safe conclusion: DO NOT treat this as evidence the merchant was given a
/// way to comply. It fails toward not suspending, which is the direction this has to
/// fail in. Our faults must not cost the merchant.
///
/// Sending `x-account-checkout-url: none` would look tidier and would break that.
/// Present-with-`none` and absent would then mean different things, a stripped header
/// would become indistinguishable from an old build, and a consumer would face three
/// states with no safe default. One signal with one safe reading beats three with
/// none.
pub static X_ACCOUNT_CHECKOUT_URL: HeaderName = HeaderName::from_static("x-account-checkout-url");

/// Written by the auth extractor with the authenticated account.
pub type AccountSlot = Arc<Mutex<Option<Uuid>>>;

/// A warning, and SEPARATELY the route that would let the merchant act on it.
///
/// THE ROUTE IS A FIELD RATHER THAN PROSE INSIDE `text`. The difference between
/// "renew at https://..." and "renew your subscription" is the difference between a
/// warning a merchant can act on and one they cannot - and anything that later treats
/// a delivered warning as evidence (a suspension, a grace-period decision) has to be
/// able to tell those apart WITHOUT STRING-MATCHING ENGLISH. Prose is edited freely by
/// anyone and nothing links the edit to the reader.
///
/// This is the same ruling the renewal findings already carry: the distinction lives in
/// a structured field, never in the message. Second site, same rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// For a human. May say "renew your subscription" when there is no route.
    pub text: String,
    /// `None` means this warning gave the merchant no way to comply. A caller deciding
    /// whether they were fairly warned must branch on this and not on `text`.
    pub checkout_url: Option<String>,
}

/// The warning to put on a response, or `None` outside the warning window.
///
/// Pure, so the window's edges are testable without a clock or a database.
#[must_use]
pub fn warning_for(held: &HeldStanding, now: DateTime<Utc>) -> Option<Warning> {
    let standing = &held.standing;
    if !standing.in_good_standing {
        return None;
    }
    let ends = standing.paid_through?;
    if ends > now + Duration::days(WARN_WINDOW_DAYS)
        || ends < now - Duration::days(ENDED_NOTICE_DAYS)
    {
        return None;
    }
    let date = ends.format("%Y-%m-%d");
    let remedy = standing.checkout_url.as_deref().map_or_else(
        || "renew your subscription".to_string(),
        |u| format!("renew at {u}"),
    );
    Some(Warning {
        text: if ends > now {
            format!("subscription period ends {date}; {remedy}")
        } else {
            format!("subscription period ended {date}; {remedy}")
        },
        checkout_url: standing.checkout_url.clone(),
    })
}

pub struct StandingWarningState {
    store: Arc<dyn AccountStandingStore>,
    cache: Mutex<HashMap<Uuid, (Instant, Option<Warning>)>>,
}

impl StandingWarningState {
    pub fn new(store: Arc<dyn AccountStandingStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn warning(&self, account: Uuid) -> Option<Warning> {
        if let Ok(cache) = self.cache.lock()
            && let Some((at, warning)) = cache.get(&account)
            && at.elapsed() < CACHE_TTL
        {
            return warning.clone();
        }
        // A failed read is logged and cached as "no warning" for the TTL, so
        // an unhealthy database costs one read per account per TTL rather
        // than one per request.
        let warning = match self.store.get_account_standing(account).await {
            Ok(held) => held.and_then(|h| warning_for(&h, Utc::now())),
            Err(e) => {
                tracing::warn!(%account, error = %e, "could not read standing for the account warning header");
                None
            }
        };
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.clear();
            }
            cache.insert(account, (Instant::now(), warning.clone()));
        }
        warning
    }
}

/// Install the slot, run the handler, then stamp the header if the request
/// was authenticated as an account inside its warning window.
pub async fn middleware(
    State(state): State<Arc<StandingWarningState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let slot: AccountSlot = Arc::new(Mutex::new(None));
    req.extensions_mut().insert(slot.clone());
    let mut response = next.run(req).await;
    let account = slot.lock().ok().and_then(|g| *g);
    if let Some(account) = account
        && let Some(warning) = state.warning(account).await
    {
        match HeaderValue::from_str(&warning.text) {
            Ok(value) => {
                response.headers_mut().insert(&X_ACCOUNT_WARNING, value);
            }
            Err(e) => {
                tracing::warn!(%account, error = %e, "account warning text is not a valid header value; header omitted");
            }
        }
        // Emitted only when a route exists. The store's column admits only https
        // URLs, so a rejection here means the value was corrupt rather than merely
        // absent, and saying so beats a silently routeless warning.
        if let Some(url) = warning.checkout_url.as_deref() {
            match HeaderValue::from_str(url) {
                Ok(value) => {
                    response
                        .headers_mut()
                        .insert(&X_ACCOUNT_CHECKOUT_URL, value);
                }
                Err(e) => {
                    tracing::warn!(%account, error = %e, "account checkout url is not a valid header value; header omitted");
                }
            }
        }
    }
    response
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test-only assertions"
)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::{Router, body::Body, routing::get};
    use data_service::{AccountStanding, ApplyOutcome};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::util::ServiceExt;
    use types::RepositoryResult;

    fn held(good: bool, paid_through: Option<DateTime<Utc>>, url: Option<&str>) -> HeldStanding {
        HeldStanding {
            standing: AccountStanding {
                account_id: Uuid::nil(),
                version: 1,
                in_good_standing: good,
                paid_through,
                plan_name: "p".into(),
                checkout_url: url.map(str::to_string),
            },
            last_heard_at: Utc::now(),
        }
    }

    #[test]
    fn warns_inside_the_window_naming_date_and_remedy() {
        let now = Utc::now();
        let h = held(
            true,
            Some(now + Duration::days(3)),
            Some("https://pay.example/c"),
        );
        let w = warning_for(&h, now).unwrap();
        assert!(
            w.text
                .contains(&(now + Duration::days(3)).format("%Y-%m-%d").to_string())
        );
        assert!(w.text.contains("renew at https://pay.example/c"));
        // The route is a FIELD, not only prose. A caller must be able to see that a
        // route was given without matching English.
        assert_eq!(w.checkout_url.as_deref(), Some("https://pay.example/c"));
    }

    /// BOTH DIRECTIONS OF THE ONE THING THAT MATTERS: a warning with a route and a
    /// warning without one must be distinguishable by the FIELD. A test over only the
    /// routed case cannot tell this implementation from one that always reports a
    /// route, and that is the direction which would make a routeless warning look
    /// actionable to anything reading it later.
    #[test]
    fn a_routeless_warning_says_so_in_the_field_not_only_in_the_prose() {
        let now = Utc::now();
        let routed = warning_for(
            &held(
                true,
                Some(now + Duration::days(2)),
                Some("https://pay.example/c"),
            ),
            now,
        )
        .unwrap();
        let routeless = warning_for(&held(true, Some(now + Duration::days(2)), None), now).unwrap();

        assert_eq!(
            routed.checkout_url.as_deref(),
            Some("https://pay.example/c")
        );
        assert_eq!(
            routeless.checkout_url, None,
            "a warning with no checkout url must report None, so a suspension path can \
             refuse to treat it as evidence the merchant was given a way to comply"
        );

        // Both still warn: silence is worse than a vague warning, because a merchant
        // told nothing discovers the lapse at the moment of failure.
        assert!(routeless.text.contains("renew your subscription"));
        assert!(routed.text.contains("renew at"));

        // And the two are not distinguishable ONLY by prose: the fields differ even
        // though both texts are non-empty and both warn about the same date.
        assert_ne!(routed.checkout_url, routeless.checkout_url);
    }

    #[test]
    fn silent_outside_the_window_and_without_a_date() {
        let now = Utc::now();
        assert!(warning_for(&held(true, Some(now + Duration::days(6)), None), now).is_none());
        assert!(warning_for(&held(true, None, None), now).is_none());
    }

    #[test]
    fn a_long_lapsed_period_is_silent() {
        let now = Utc::now();
        let h = held(
            true,
            Some(now - Duration::days(ENDED_NOTICE_DAYS + 1)),
            None,
        );
        assert!(warning_for(&h, now).is_none());
    }

    #[test]
    fn silent_for_an_account_not_in_good_standing() {
        let now = Utc::now();
        assert!(warning_for(&held(false, Some(now + Duration::days(1)), None), now).is_none());
    }

    #[test]
    fn a_lapsed_period_still_in_good_standing_says_ended() {
        let now = Utc::now();
        let text = warning_for(&held(true, Some(now - Duration::days(1)), None), now)
            .unwrap()
            .text;
        assert!(text.contains("ended"));
        assert!(text.contains("renew your subscription"));
    }

    struct Counting {
        row: Option<HeldStanding>,
        reads: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl AccountStandingStore for Counting {
        async fn apply_account_standing(
            &self,
            _: &AccountStanding,
        ) -> RepositoryResult<ApplyOutcome> {
            unreachable!("the warning path never writes")
        }
        async fn get_account_standing(&self, _: Uuid) -> RepositoryResult<Option<HeldStanding>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(types::RepositoryError::Database("down".into()));
            }
            Ok(self.row.clone())
        }
    }

    /// A handler that authenticates (fills the slot) or does not.
    fn app(store: Arc<Counting>, authenticate: bool) -> Router {
        let state = Arc::new(StandingWarningState::new(store));
        Router::new()
            .route(
                "/",
                get(move |req: Request| async move {
                    if authenticate {
                        let slot = req.extensions().get::<AccountSlot>().cloned().unwrap();
                        *slot.lock().unwrap() = Some(Uuid::nil());
                    }
                    "ok"
                }),
            )
            .layer(axum::middleware::from_fn_with_state(state, middleware))
    }

    async fn call(app: &Router) -> Response {
        app.clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn store(row: Option<HeldStanding>) -> Arc<Counting> {
        Arc::new(Counting {
            row,
            reads: AtomicUsize::new(0),
            fail: false,
        })
    }

    #[tokio::test]
    async fn a_failing_store_is_read_once_per_ttl_not_per_request() {
        let s = Arc::new(Counting {
            row: None,
            reads: AtomicUsize::new(0),
            fail: true,
        });
        let app = app(s.clone(), true);
        for _ in 0..3 {
            assert!(call(&app).await.headers().get(&X_ACCOUNT_WARNING).is_none());
        }
        assert_eq!(s.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stamps_inside_the_window_and_reads_the_store_once() {
        let s = store(Some(held(true, Some(Utc::now() + Duration::days(2)), None)));
        let app = app(s.clone(), true);
        for _ in 0..3 {
            let r = call(&app).await;
            assert!(r.headers().get(&X_ACCOUNT_WARNING).is_some());
        }
        assert_eq!(s.reads.load(Ordering::SeqCst), 1, "answered from cache");
    }

    #[tokio::test]
    async fn adds_nothing_outside_the_window() {
        let s = store(Some(held(
            true,
            Some(Utc::now() + Duration::days(20)),
            None,
        )));
        let r = call(&app(s, true)).await;
        assert!(r.headers().get(&X_ACCOUNT_WARNING).is_none());
    }

    #[tokio::test]
    async fn an_unauthenticated_request_reads_nothing_and_gets_nothing() {
        let s = store(Some(held(true, Some(Utc::now() + Duration::days(2)), None)));
        let r = call(&app(s.clone(), false)).await;
        assert!(r.headers().get(&X_ACCOUNT_WARNING).is_none());
        assert_eq!(s.reads.load(Ordering::SeqCst), 0);
    }
}
