//! The authenticated merchant's own account standing.
//!
//! Email is optional, so the interface is the only channel every merchant is
//! guaranteed to have. Without this read a merchant learns their standing
//! lapsed only by being refused an invoice. The standing is pushed by an
//! external service and stored by the host; this module only reports it, and
//! decides nothing that the invoice gate does not already decide.

use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Duration, Utc};
use data_service::{AccountStandingStore, HeldStanding};
use serde::Serialize;
use utoipa::ToSchema;

use auth::SessionService;

use crate::api::extractors::AuthenticatedUser;
use crate::state::PgAppState;

/// How far ahead of the paid-through date a merchant is warned.
const WARN_BEFORE_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StandingState {
    /// Nothing was ever received for this account. Nothing to show.
    Unknown,
    /// In good standing, with no lapse close.
    Good,
    /// In good standing, but paid through a date that is near or past.
    Expiring,
    /// Not in good standing: new invoices are refused.
    Lapsed,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StandingResponse {
    pub state: StandingState,
    pub plan_name: Option<String>,
    pub paid_through: Option<DateTime<Utc>>,
    /// Where to put it right. The store's column constraint admits only `https`
    /// URLs, so this is one whenever present.
    pub checkout_url: Option<String>,
    /// ABLATION ONLY - a field added without amending the allow-list.
    pub ablation_leak: Option<String>,
}

fn classify(held: Option<&HeldStanding>, now: DateTime<Utc>) -> StandingResponse {
    let Some(held) = held else {
        return StandingResponse {
            state: StandingState::Unknown,
            plan_name: None,
            paid_through: None,
            checkout_url: None,
            ablation_leak: None,
        };
    };
    let s = &held.standing;
    let state = if !s.in_good_standing {
        StandingState::Lapsed
    } else if s
        .paid_through
        .is_some_and(|t| t - now <= Duration::days(WARN_BEFORE_DAYS))
    {
        StandingState::Expiring
    } else {
        StandingState::Good
    };
    StandingResponse {
        state,
        plan_name: Some(s.plan_name.clone()),
        paid_through: s.paid_through,
        checkout_url: s.checkout_url.clone(),
        ablation_leak: Some("leaked".into()),
    }
}

/// Report the caller's own standing, so the client can warn before a lapse
/// and name the remedy after one.
#[utoipa::path(
    get,
    path = "/users/me/standing",
    tag = "users",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "The caller's account standing", body = StandingResponse),
        (status = 401, description = "Unauthorized"),
        (status = 503, description = "Database unavailable; retry"),
    )
)]
pub async fn get_standing<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Result<Json<StandingResponse>, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    // Read only, and never through `standing_decision`: that surfaces
    // fail-open allows as an operator signal, and a merchant viewing a banner
    // is not an invoice being let through.
    let held = state
        .data_service
        .get_account_standing(user.id.0)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "account standing store unavailable");
            (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable")
        })?;
    Ok(Json(classify(held.as_ref(), Utc::now())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_service::AccountStanding;
    use uuid::Uuid;

    fn held(good: bool, paid_through: Option<DateTime<Utc>>) -> HeldStanding {
        HeldStanding {
            standing: AccountStanding {
                account_id: Uuid::nil(),
                version: 1,
                in_good_standing: good,
                paid_through,
                plan_name: "p".into(),
                checkout_url: Some("https://pay.example/c".into()),
            },
            last_heard_at: Utc::now(),
        }
    }

    #[test]
    fn nothing_held_is_unknown() {
        assert_eq!(classify(None, Utc::now()).state, StandingState::Unknown);
    }

    #[test]
    fn lapsed_carries_the_remedy_link() {
        let r = classify(Some(&held(false, None)), Utc::now());
        assert_eq!(r.state, StandingState::Lapsed);
        assert_eq!(r.checkout_url.as_deref(), Some("https://pay.example/c"));
    }

    #[test]
    fn warns_inside_the_window_and_not_outside_it() {
        let now = Utc::now();
        let near = classify(Some(&held(true, Some(now + Duration::days(3)))), now);
        assert_eq!(near.state, StandingState::Expiring);
        let far = classify(Some(&held(true, Some(now + Duration::days(30)))), now);
        assert_eq!(far.state, StandingState::Good);
        let open_ended = classify(Some(&held(true, None)), now);
        assert_eq!(open_ended.state, StandingState::Good);
    }

    #[test]
    fn the_warning_window_includes_its_boundary() {
        let now = Utc::now();
        let on = classify(
            Some(&held(true, Some(now + Duration::days(WARN_BEFORE_DAYS)))),
            now,
        );
        assert_eq!(on.state, StandingState::Expiring);
        let past = Duration::days(WARN_BEFORE_DAYS) + Duration::seconds(1);
        let after = classify(Some(&held(true, Some(now + past))), now);
        assert_eq!(after.state, StandingState::Good);
    }

    #[test]
    fn past_paid_through_but_still_good_is_a_warning_not_good() {
        let now = Utc::now();
        let r = classify(Some(&held(true, Some(now - Duration::days(1)))), now);
        assert_eq!(r.state, StandingState::Expiring);
    }

    /// The banner must never contradict the invoice gate: a merchant shown
    /// anything but `Lapsed` must be let through, and a `Lapsed` one refused,
    /// whatever the paid-through date and however long the sender has been
    /// silent. A past paid-through date on a good standing is allowed by the
    /// gate, so it is only a warning here.
    #[test]
    fn lapsed_exactly_when_the_invoice_gate_denies() {
        let now = Utc::now();
        let max_age = Duration::days(data_service::DEFAULT_STANDING_MAX_AGE_DAYS);
        for good in [true, false] {
            for paid_through in [
                None,
                Some(now - Duration::days(1)),
                Some(now + Duration::days(3)),
                Some(now + Duration::days(30)),
            ] {
                for silence in [Duration::zero(), max_age + Duration::days(1)] {
                    let mut h = held(good, paid_through);
                    h.last_heard_at = now - silence;
                    let shown = classify(Some(&h), now).state;
                    let denied = matches!(
                        data_service::decide_and_surface(Uuid::nil(), Some(&h), now, max_age),
                        data_service::StandingDecision::Deny { .. }
                    );
                    assert_eq!(
                        shown == StandingState::Lapsed,
                        denied,
                        "good={good} paid_through={paid_through:?} silence={silence:?}"
                    );
                }
            }
        }
    }
}
