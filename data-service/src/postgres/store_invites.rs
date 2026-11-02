//! Postgres-backed store invites. See `crate::store_invites`.

use async_trait::async_trait;
use auth::{StoreId, StoreRoleId, UserId};
use chrono::{DateTime, Utc};
use sqlx::Row;
use types::{RepositoryError, RepositoryResult};
use uuid::Uuid;

use crate::postgres::PgDataService;
use crate::store_invites::{InviteAcceptance, StoreInviteWriter};

fn db(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Database(e.to_string())
}

/// The audit label stored for an outcome. The CHECK constraint on
/// `store_invite_attempts.outcome` must list the same four strings.
fn outcome_label(outcome: &InviteAcceptance) -> &'static str {
    match outcome {
        InviteAcceptance::Accepted { .. } => "accepted",
        InviteAcceptance::AlreadyMember => "already_member",
        InviteAcceptance::EmailMismatch => "email_mismatch",
        InviteAcceptance::Invalid => "invalid",
    }
}

/// Emails are stored lower-cased and trimmed by the writer, so the caller's
/// address is normalised the same way before comparison.
fn normalise_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[async_trait]
impl StoreInviteWriter for PgDataService {
    async fn create_store_invite(
        &self,
        store_id: StoreId,
        email: &str,
        role_id: StoreRoleId,
        invited_by: UserId,
        expires_at: DateTime<Utc>,
    ) -> RepositoryResult<Uuid> {
        // One atomic statement against the partial unique index, so a
        // re-invite (or a double click) replaces the token instead of leaving
        // two live ones.
        let row = sqlx::query(
            "INSERT INTO store_invites (store_id, email, store_role_id, invited_by, expires_at) \
             VALUES ($1, lower(btrim($2)), $3, $4, $5) \
             ON CONFLICT (store_id, email) WHERE accepted_at IS NULL \
             DO UPDATE SET store_role_id = EXCLUDED.store_role_id, \
                 invited_by = EXCLUDED.invited_by, \
                 expires_at = EXCLUDED.expires_at, \
                 created_at = NOW(), \
                 token = uuid_generate_v4() \
             RETURNING token",
        )
        .bind(store_id.0)
        .bind(email)
        .bind(role_id.0)
        .bind(invited_by.0)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(row.get("token"))
    }

    async fn accept_store_invite(
        &self,
        token: Uuid,
        user_id: UserId,
        user_email: Option<&str>,
    ) -> RepositoryResult<InviteAcceptance> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        // Take the invite. The WHERE clause is the whole validity check, and
        // the row lock serialises two concurrent redemptions of one token.
        let invite = sqlx::query(
            "SELECT store_id, store_role_id, email FROM store_invites \
             WHERE token = $1 AND accepted_at IS NULL AND expires_at > NOW() \
             FOR UPDATE",
        )
        .bind(token)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;

        let (outcome, store_id) = match invite {
            None => (InviteAcceptance::Invalid, None),
            Some(invite) => {
                let store_id: Uuid = invite.get("store_id");
                let role_id: Uuid = invite.get("store_role_id");
                let invited_email: String = invite.get("email");

                let already: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM user_stores WHERE user_id = $1 AND store_id = $2)",
                )
                .bind(user_id.0)
                .bind(store_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;

                if already {
                    (InviteAcceptance::AlreadyMember, Some(store_id))
                } else if user_email.is_some_and(|e| normalise_email(e) != invited_email) {
                    // A wallet-only account (no email) is not compared.
                    (InviteAcceptance::EmailMismatch, Some(store_id))
                } else {
                    sqlx::query(
                        "INSERT INTO user_stores (user_id, store_id, store_role_id) VALUES ($1, $2, $3)",
                    )
                    .bind(user_id.0)
                    .bind(store_id)
                    .bind(role_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                    // One statement: accepted_by is recorded atomically with
                    // accepted_at, never as a second write that could land
                    // without the other. The `accepted_at IS NULL` guard is
                    // belt-and-suspenders - the row lock above already
                    // serialises this against a second redemption - but it
                    // means this statement can never overwrite a redeemer
                    // already recorded for this token.
                    sqlx::query(
                        "UPDATE store_invites SET accepted_at = NOW(), accepted_by = $2 \
                         WHERE token = $1 AND accepted_at IS NULL",
                    )
                    .bind(token)
                    .bind(user_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                    (
                        InviteAcceptance::Accepted {
                            store_id: StoreId(store_id),
                            role_id: StoreRoleId(role_id),
                        },
                        Some(store_id),
                    )
                }
            }
        };

        // Written whatever the outcome, in the same transaction as the
        // decision, so a refusal is as durable as a membership.
        sqlx::query(
            "INSERT INTO store_invite_attempts (token, store_id, user_id, outcome) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(token)
        .bind(store_id)
        .bind(user_id.0)
        .bind(outcome_label(&outcome))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;

        Ok(outcome)
    }

    async fn invite_redeemers(&self, store_id: StoreId) -> RepositoryResult<Vec<UserId>> {
        let rows: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT accepted_by FROM store_invites \
             WHERE store_id = $1 AND accepted_by IS NOT NULL",
        )
        .bind(store_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows.into_iter().map(UserId).collect())
    }
}
