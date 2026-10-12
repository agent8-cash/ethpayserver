//! An account's standing, pushed here by an external service.
//!
//! The standing gates invoice creation, so the two properties that matter are
//! kept in the data layer where a real database can prove them: a lower
//! version never replaces a higher one, and an absent or silent sender is
//! never mistaken for a healthy one without saying so.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use types::RepositoryResult;
use uuid::Uuid;

/// How long the sender may stay silent about an account before an allow
/// decision based on what we hold is reported as stale.
pub const DEFAULT_STANDING_MAX_AGE_DAYS: i64 = 7;

/// A standing as the sender pushed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountStanding {
    pub account_id: Uuid,
    pub version: i64,
    pub in_good_standing: bool,
    pub paid_through: Option<DateTime<Utc>>,
    pub plan_name: String,
    pub checkout_url: Option<String>,
}

/// A held standing and when the sender last confirmed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldStanding {
    pub standing: AccountStanding,
    pub last_heard_at: DateTime<Utc>,
}

/// What applying a push did. Both arms are success to the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The pushed version was higher and is now held.
    Applied { version: i64 },
    /// An equal or higher version was already held; nothing was replaced.
    Kept { held_version: i64 },
}

/// Whether to let an invoice be created, and if so, on what footing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandingDecision {
    /// Heard from recently, and in good standing.
    Allow,
    /// In good standing as of a push, but the sender has not confirmed it
    /// within the freshness bound. Allowed, and surfaced.
    AllowStale { silent_for: Duration },
    /// Nothing was ever pushed for this account. Allowed, and surfaced.
    AllowUnheard,
    /// Not in good standing. The invoice is refused.
    Deny {
        plan_name: String,
        checkout_url: Option<String>,
    },
}

impl StandingDecision {
    /// An allow that rests on missing or old information rather than on a
    /// confirmed good standing.
    pub fn is_fail_open(&self) -> bool {
        matches!(self, Self::AllowStale { .. } | Self::AllowUnheard)
    }
}

/// Decide from what is held. Pure, so the freshness bound is testable without
/// a clock or a database.
///
/// A known-bad standing denies however old it is: the sender's last word on
/// that account was "no", and silence is not a reason to revise it.
pub fn decide(
    held: Option<&HeldStanding>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> StandingDecision {
    let Some(held) = held else {
        return StandingDecision::AllowUnheard;
    };
    if !held.standing.in_good_standing {
        return StandingDecision::Deny {
            plan_name: held.standing.plan_name.clone(),
            checkout_url: held.standing.checkout_url.clone(),
        };
    }
    let silent_for = now - held.last_heard_at;
    if silent_for > max_age {
        StandingDecision::AllowStale { silent_for }
    } else {
        StandingDecision::Allow
    }
}

#[async_trait]
pub trait AccountStandingStore: Send + Sync {
    /// Compare-and-set on `version`: replaces the held standing only if the
    /// pushed version is strictly higher. An equal version also records that
    /// the sender was heard, without touching the held content.
    async fn apply_account_standing(
        &self,
        standing: &AccountStanding,
    ) -> RepositoryResult<ApplyOutcome>;

    async fn get_account_standing(
        &self,
        account_id: Uuid,
    ) -> RepositoryResult<Option<HeldStanding>>;

    /// Read and decide, surfacing every fail-open decision (see
    /// [`decide_and_surface`]).
    async fn standing_decision(
        &self,
        account_id: Uuid,
        max_age: Duration,
    ) -> RepositoryResult<StandingDecision> {
        let held = self.get_account_standing(account_id).await?;
        Ok(decide_and_surface(
            account_id,
            held.as_ref(),
            Utc::now(),
            max_age,
        ))
    }
}

/// Counter incremented once per fail-open allow, labelled by `reason`.
pub const FAIL_OPEN_COUNTER: &str = "ethpayserver_standing_fail_open_total";

/// [`decide`], and make every fail-open allow reach a person.
///
/// A fail-open allow lets a possibly-lapsed account keep creating invoices on
/// the strength of missing or old information. A `warn!` is a breadcrumb that
/// reaches nobody, so each one is an error-level event (which the error
/// reporter turns into an alert) and increments [`FAIL_OPEN_COUNTER`] so the
/// rate can be alarmed on as well. Every caller that reads a standing to
/// decide anything goes through here, so none can allow silently.
pub fn decide_and_surface(
    account_id: Uuid,
    held: Option<&HeldStanding>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> StandingDecision {
    let decision = decide(held, now, max_age);
    match &decision {
        StandingDecision::AllowUnheard => {
            metrics::counter!(FAIL_OPEN_COUNTER, "reason" => "unheard").increment(1);
            tracing::error!(%account_id, "standing fail-open: no standing was ever received for this account, allowing");
        }
        StandingDecision::AllowStale { silent_for } => {
            metrics::counter!(FAIL_OPEN_COUNTER, "reason" => "stale").increment(1);
            tracing::error!(
                %account_id,
                silent_for_secs = silent_for.num_seconds(),
                max_age_secs = max_age.num_seconds(),
                "standing fail-open: sender not heard from within the freshness bound, allowing"
            );
        }
        StandingDecision::Allow | StandingDecision::Deny { .. } => {}
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(good: bool, heard: DateTime<Utc>) -> HeldStanding {
        HeldStanding {
            standing: AccountStanding {
                account_id: Uuid::nil(),
                version: 1,
                in_good_standing: good,
                paid_through: None,
                plan_name: "p".into(),
                checkout_url: Some("https://x.example/c".into()),
            },
            last_heard_at: heard,
        }
    }

    #[test]
    fn no_row_allows_and_is_fail_open() {
        let d = decide(None, Utc::now(), Duration::days(7));
        assert_eq!(d, StandingDecision::AllowUnheard);
        assert!(d.is_fail_open());
    }

    #[test]
    fn a_recent_good_standing_allows_without_being_fail_open() {
        let now = Utc::now();
        let d = decide(
            Some(&held(true, now - Duration::days(1))),
            now,
            Duration::days(7),
        );
        assert_eq!(d, StandingDecision::Allow);
        assert!(!d.is_fail_open());
    }

    #[test]
    fn a_good_standing_not_confirmed_within_the_bound_is_allowed_but_stale() {
        let now = Utc::now();
        let d = decide(
            Some(&held(true, now - Duration::days(8))),
            now,
            Duration::days(7),
        );
        assert!(matches!(d, StandingDecision::AllowStale { .. }));
        assert!(d.is_fail_open());
    }

    #[test]
    fn a_bad_standing_denies_however_old_it_is() {
        let now = Utc::now();
        for age in [1, 30, 3650] {
            let d = decide(
                Some(&held(false, now - Duration::days(age))),
                now,
                Duration::days(7),
            );
            assert!(matches!(d, StandingDecision::Deny { .. }), "age {age}d");
        }
    }
}
