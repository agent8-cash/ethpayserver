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
    ) -> RepositoryResult<InviteAcceptance> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        // Take the invite. The WHERE clause is the whole validity check, and
        // the row lock serialises two concurrent redemptions of one token.
        let Some(invite) = sqlx::query(
            "SELECT store_id, store_role_id FROM store_invites \
             WHERE token = $1 AND accepted_at IS NULL AND expires_at > NOW() \
             FOR UPDATE",
        )
        .bind(token)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        else {
            return Ok(InviteAcceptance::Invalid);
        };
        let store_id: Uuid = invite.get("store_id");
        let role_id: Uuid = invite.get("store_role_id");

        let already: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM user_stores WHERE user_id = $1 AND store_id = $2)",
        )
        .bind(user_id.0)
        .bind(store_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if already {
            return Ok(InviteAcceptance::AlreadyMember);
        }

        sqlx::query(
            "INSERT INTO user_stores (user_id, store_id, store_role_id) VALUES ($1, $2, $3)",
        )
        .bind(user_id.0)
        .bind(store_id)
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("UPDATE store_invites SET accepted_at = NOW() WHERE token = $1")
            .bind(token)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;

        Ok(InviteAcceptance::Accepted {
            store_id: StoreId(store_id),
            role_id: StoreRoleId(role_id),
        })
    }
}
