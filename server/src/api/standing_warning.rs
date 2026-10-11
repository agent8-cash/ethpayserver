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

/// How long one account's answer is reused before the standing is read again.
pub const CACHE_TTL: StdDuration = StdDuration::from_secs(60);

/// Entries kept before the cache is emptied, so it cannot grow without bound.
const MAX_CACHE_ENTRIES: usize = 10_000;

pub static X_ACCOUNT_WARNING: HeaderName = HeaderName::from_static("x-account-warning");

/// Written by the auth extractor with the authenticated account.
pub type AccountSlot = Arc<Mutex<Option<Uuid>>>;

/// The text to put on a response, or `None` outside the warning window.
///
/// Pure, so the window's edges are testable without a clock or a database.
#[must_use]
pub fn warning_for(held: &HeldStanding, now: DateTime<Utc>) -> Option<String> {
    let standing = &held.standing;
    if !standing.in_good_standing {
        return None;
    }
    let ends = standing.paid_through?;
    if ends > now + Duration::days(WARN_WINDOW_DAYS) {
        return None;
    }
    let date = ends.format("%Y-%m-%d");
    let remedy = standing.checkout_url.as_deref().map_or_else(
        || "renew your subscription".to_string(),
        |u| format!("renew at {u}"),
    );
    Some(if ends > now {
        format!("subscription period ends {date}; {remedy}")
    } else {
        format!("subscription period ended {date}; {remedy}")
    })
}

pub struct StandingWarningState {
    store: Arc<dyn AccountStandingStore>,
    cache: Mutex<HashMap<Uuid, (Instant, Option<String>)>>,
}

impl StandingWarningState {
    pub fn new(store: Arc<dyn AccountStandingStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn warning(&self, account: Uuid) -> Option<String> {
        if let Ok(cache) = self.cache.lock()
            && let Some((at, text)) = cache.get(&account)
            && at.elapsed() < CACHE_TTL
        {
            return text.clone();
        }
        // A failed read is not cached and not reported: this is a courtesy
        // header, and the next request tries again.
        let held = self.store.get_account_standing(account).await.ok()?;
        let text = held.and_then(|h| warning_for(&h, Utc::now()));
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.clear();
            }
            cache.insert(account, (Instant::now(), text.clone()));
        }
        text
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
        && let Some(text) = state.warning(account).await
        && let Ok(value) = HeaderValue::from_str(&text)
    {
        response.headers_mut().insert(&X_ACCOUNT_WARNING, value);
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
        let text = warning_for(&h, now).unwrap();
        assert!(text.contains(&(now + Duration::days(3)).format("%Y-%m-%d").to_string()));
        assert!(text.contains("renew at https://pay.example/c"));
    }

    #[test]
    fn silent_outside_the_window_and_without_a_date() {
        let now = Utc::now();
        assert!(warning_for(&held(true, Some(now + Duration::days(6)), None), now).is_none());
        assert!(warning_for(&held(true, None, None), now).is_none());
    }

    #[test]
    fn silent_for_an_account_not_in_good_standing() {
        let now = Utc::now();
        assert!(warning_for(&held(false, Some(now + Duration::days(1)), None), now).is_none());
    }

    #[test]
    fn a_lapsed_period_still_in_good_standing_says_ended() {
        let now = Utc::now();
        let text = warning_for(&held(true, Some(now - Duration::days(1)), None), now).unwrap();
        assert!(text.contains("ended"));
        assert!(text.contains("renew your subscription"));
    }

    struct Counting {
        row: Option<HeldStanding>,
        reads: AtomicUsize,
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
        })
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
