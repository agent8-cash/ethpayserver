//! UserRepository implementation.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;

use auth::{
    User, UserId, UserRepository,
    error::{AuthError, Result},
};

use super::{PgDataService, sqlx_to_auth_error};

#[async_trait]
impl UserRepository for PgDataService {
    /// Create an account.
    ///
    /// THE FIRST ACCOUNT ON A SERVER BECOMES ITS ADMINISTRATOR, which is the
    /// rule BTCPay Server uses and the reason it needs no bootstrap step. Before
    /// this, nothing in the codebase ever wrote `server_admin`: the role could
    /// only be granted through the admin UI, which requires an existing admin to
    /// reach, so a fresh database had no path to one and plugin install, user
    /// management and server settings were unreachable. Testnet hit that twice in
    /// two days and both times the way out was UPDATE typed against production.
    ///
    /// THE DECISION IS IN THE INSERT, not read-then-write around it. A COUNT
    /// followed by an insert is two statements with a gap, and the gap is on the
    /// one code path where being wrong grants server administration. As a CASE
    /// inside the statement the emptiness test is evaluated against the same
    /// snapshot that does the insert.
    ///
    /// IT ONLY EVER UPGRADES THE DEFAULT ROLE. An explicit `server_admin` passes
    /// through untouched, so test fixtures and any future "create this user as
    /// X" path keep saying what they mean, and a plain `user` is never silently
    /// demoted.
    ///
    /// TWO THINGS IT DOES NOT DO, both deliberate:
    ///
    /// - It does not keep `user.role` in step. The caller's struct still says
    ///   whatever it said, so a registration response can report `user` for the
    ///   account that just became an admin. Every authorisation decision reads
    ///   the ROW (`validate_api_key`, the session lookup), so the grant is in
    ///   force on the next request and the staleness is cosmetic. Fixing it
    ///   properly means `create_user` returning the effective role, which is a
    ///   signature in `payserver-commons` and a pinned-revision change.
    /// - It does not close the window on a PUBLIC server. Between a fresh deploy
    ///   and the operator registering, whoever registers first becomes the
    ///   administrator; and two registrations racing on an empty table can see no
    ///   rows each and both be promoted, since neither has committed when the
    ///   other evaluates the test. On an instance that is not yet public that
    ///   window is not open. Closing it needs a bootstrap that is not
    ///   first-come - a token minted at first start, or a deploy-time flag - and
    ///   that is a decision about mainnet rather than a detail of this function.
    async fn create_user(&self, user: &User) -> Result<()> {
        let kdf_params = serde_json::to_value(&user.kdf_params)
            .map_err(|e| AuthError::Repository(e.to_string()))?;
        let encrypted_key = serde_json::to_value(&user.encrypted_symmetric_key)
            .map_err(|e| AuthError::Repository(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO users (
                id, email, primary_wallet_address, kdf_salt_identifier, kdf_params,
                encrypted_symmetric_key, recovery_verification_hash, created_at,
                last_login_at, failed_login_attempts, locked_until, role
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                CASE
                    WHEN $12 = 'user' AND NOT EXISTS (SELECT 1 FROM users)
                        THEN 'server_admin'
                    ELSE $12
                END)
            "#,
        )
        .bind(user.id.0)
        .bind(&user.email)
        .bind(&user.primary_wallet_address)
        .bind(&user.kdf_salt_identifier)
        .bind(&kdf_params)
        .bind(&encrypted_key)
        .bind(&user.recovery_verification_hash)
        .bind(user.created_at)
        .bind(user.last_login_at)
        .bind(user.failed_login_attempts as i32)
        .bind(user.locked_until)
        .bind(user.role.as_str())
        .execute(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        Ok(())
    }

    async fn get_user(&self, id: UserId) -> Result<Option<User>> {
        let row = sqlx::query(
            r#"
            SELECT id, email, primary_wallet_address, kdf_salt_identifier, kdf_params,
                   encrypted_symmetric_key, recovery_verification_hash, created_at,
                   last_login_at, failed_login_attempts, locked_until, role
            FROM users WHERE id = $1
            "#,
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        row.map(|r| row_to_user(&r)).transpose()
    }

    async fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let row = sqlx::query(
            r#"
            SELECT id, email, primary_wallet_address, kdf_salt_identifier, kdf_params,
                   encrypted_symmetric_key, recovery_verification_hash, created_at,
                   last_login_at, failed_login_attempts, locked_until, role
            FROM users WHERE LOWER(email) = LOWER($1)
            "#,
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        row.map(|r| row_to_user(&r)).transpose()
    }

    async fn get_user_by_wallet_address(&self, address: &str) -> Result<Option<User>> {
        let row = sqlx::query(
            r#"
            SELECT id, email, primary_wallet_address, kdf_salt_identifier, kdf_params,
                   encrypted_symmetric_key, recovery_verification_hash, created_at,
                   last_login_at, failed_login_attempts, locked_until, role
            FROM users WHERE primary_wallet_address = $1
            "#,
        )
        .bind(address)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        row.map(|r| row_to_user(&r)).transpose()
    }

    async fn update_user(&self, user: &User) -> Result<()> {
        let kdf_params = serde_json::to_value(&user.kdf_params)
            .map_err(|e| AuthError::Repository(e.to_string()))?;
        let encrypted_key = serde_json::to_value(&user.encrypted_symmetric_key)
            .map_err(|e| AuthError::Repository(e.to_string()))?;

        let result = sqlx::query(
            r#"
            -- kdf_salt_identifier is deliberately absent: it is pinned at
            -- registration and the stored recovery_verification_hash was
            -- derived from it. Updating it would make the account
            -- unrecoverable, so this statement cannot.
            UPDATE users SET
                -- COALESCE, not assignment: pins the value on first write for
                -- rows the old binary inserted during a rolling deploy (which
                -- the one-shot backfill cannot reach), while remaining
                -- immutable for every row that already has one. Without this
                -- those rows keep recompute-on-read semantics forever and the
                -- promised follow-up SET NOT NULL would find NULLs.
                kdf_salt_identifier = COALESCE(users.kdf_salt_identifier, $11),
                email = $2, primary_wallet_address = $3, kdf_params = $4,
                encrypted_symmetric_key = $5, recovery_verification_hash = $6,
                last_login_at = $7, failed_login_attempts = $8, locked_until = $9,
                role = $10
            WHERE id = $1
              -- Reject a changed identifier instead of silently discarding it.
              -- COALESCE above pins a NULL row, which is why NULL still
              -- matches; anything else must equal what is stored, or no row is
              -- updated and the caller is told. Without this the
              -- write looked successful and the change simply evaporated.
              AND (users.kdf_salt_identifier IS NULL
                   OR users.kdf_salt_identifier = $11)
            "#,
        )
        .bind(user.id.0)
        .bind(&user.email)
        .bind(&user.primary_wallet_address)
        .bind(&kdf_params)
        .bind(&encrypted_key)
        .bind(&user.recovery_verification_hash)
        .bind(user.last_login_at)
        .bind(user.failed_login_attempts as i32)
        .bind(user.locked_until)
        .bind(user.role.as_str())
        .bind(&user.kdf_salt_identifier)
        .execute(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        if result.rows_affected() == 0 {
            // Zero rows now has two causes, and reporting the wrong one sends
            // the reader hunting for a user that is sitting right there. Ask.
            let existing: Option<Option<String>> =
                sqlx::query_scalar("SELECT kdf_salt_identifier FROM users WHERE id = $1")
                    .bind(user.id.0)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(sqlx_to_auth_error)?;

            return match existing {
                Some(_) => Err(AuthError::ImmutableField("kdf_salt_identifier".to_string())),
                None => Err(AuthError::UserNotFound(user.id.to_string())),
            };
        }
        Ok(())
    }

    async fn delete_user(&self, id: UserId) -> Result<()> {
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_auth_error)?;
        Ok(())
    }

    async fn increment_failed_logins(&self, id: UserId) -> Result<u32> {
        let row = sqlx::query(
            r#"
            UPDATE users SET failed_login_attempts = failed_login_attempts + 1
            WHERE id = $1
            RETURNING failed_login_attempts
            "#,
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        match row {
            Some(r) => {
                let count: i32 = r.get("failed_login_attempts");
                Ok(count as u32)
            }
            None => Err(AuthError::UserNotFound(id.to_string())),
        }
    }

    async fn reset_failed_logins(&self, id: UserId) -> Result<()> {
        let result = sqlx::query("UPDATE users SET failed_login_attempts = 0 WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_auth_error)?;

        if result.rows_affected() == 0 {
            return Err(AuthError::UserNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn lock_user(&self, id: UserId, until: DateTime<Utc>) -> Result<()> {
        let result = sqlx::query("UPDATE users SET locked_until = $2 WHERE id = $1")
            .bind(id.0)
            .bind(until)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_auth_error)?;

        if result.rows_affected() == 0 {
            return Err(AuthError::UserNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn unlock_user(&self, id: UserId) -> Result<()> {
        let result = sqlx::query("UPDATE users SET locked_until = NULL WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_auth_error)?;

        if result.rows_affected() == 0 {
            return Err(AuthError::UserNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn list_users(&self, offset: i64, limit: i64) -> Result<Vec<User>> {
        let rows = sqlx::query(
            r#"
            SELECT id, email, primary_wallet_address, kdf_salt_identifier, kdf_params,
                   encrypted_symmetric_key, recovery_verification_hash, created_at,
                   last_login_at, failed_login_attempts, locked_until, role
            FROM users ORDER BY created_at DESC LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_auth_error)?;

        rows.iter().map(row_to_user).collect()
    }

    async fn count_users(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as count FROM users")
            .fetch_one(&self.pool)
            .await
            .map_err(sqlx_to_auth_error)?;
        Ok(row.get::<i64, _>("count"))
    }
}

fn row_to_user(row: &sqlx::postgres::PgRow) -> Result<User> {
    let kdf_params_json: serde_json::Value = row.get("kdf_params");
    let encrypted_key_json: serde_json::Value = row.get("encrypted_symmetric_key");
    let failed_attempts: i32 = row.get("failed_login_attempts");
    let role_str: String = row.get("role");

    let id: uuid::Uuid = row.get("id");
    let email: Option<String> = row.get("email");
    let primary_wallet_address: Option<String> = row.get("primary_wallet_address");

    Ok(User {
        id: UserId(id),
        email: email.clone(),
        primary_wallet_address: primary_wallet_address.clone(),
        // NULL means the row predates the backfill (or was written by the
        // old binary during a rolling deploy). Fall back to the computed value,
        // which is what such a row was salted with anyway.
        kdf_salt_identifier: row
            .get::<Option<String>, _>("kdf_salt_identifier")
            .unwrap_or_else(|| match (&email, &primary_wallet_address) {
                (_, Some(w)) => format!("wallet:{w}"),
                // Lowercased to agree with the backfill and User::new, which
                // both pin the normalised form.
                (Some(e), None) => e.to_lowercase(),
                (None, None) => format!("passkey:{id}"),
            }),
        kdf_params: serde_json::from_value(kdf_params_json)
            .map_err(|e| AuthError::Repository(e.to_string()))?,
        encrypted_symmetric_key: serde_json::from_value(encrypted_key_json)
            .map_err(|e| AuthError::Repository(e.to_string()))?,
        recovery_verification_hash: row.get("recovery_verification_hash"),
        created_at: row.get("created_at"),
        last_login_at: row.get("last_login_at"),
        failed_login_attempts: failed_attempts as u32,
        locked_until: row.get("locked_until"),
        role: role_str.parse().unwrap_or_default(),
    })
}

#[async_trait::async_trait]
impl crate::account_deletion::AccountDeletionReader for crate::postgres::PgDataService {
    async fn account_deletion_blockers(
        &self,
        user_id: auth::UserId,
    ) -> types::RepositoryResult<crate::account_deletion::AccountDeletionBlockers> {
        // One round trip, three scalar subqueries. Each walks from the owned
        // stores down the path that `ON DELETE CASCADE` would take, so what is
        // counted here is exactly what a delete would remove or trip over.
        let row = sqlx::query(
            r#"
            SELECT
              (SELECT COUNT(*) FROM payments p
                 JOIN invoices i ON i.id = p.invoice_id
                 JOIN stores s   ON s.id = i.store_id
                WHERE s.owner_id = $1)                       AS payments,
              (SELECT COUNT(*) FROM payouts po
                 JOIN stores s ON s.id = po.store_id
                WHERE s.owner_id = $1)                       AS payouts,
              (SELECT COUNT(*) FROM refunds r
                 JOIN stores s ON s.id = r.store_id
                WHERE s.owner_id = $1)                       AS refunds
            "#,
        )
        .bind(user_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| types::RepositoryError::Database(e.to_string()))?;

        use sqlx::Row;
        Ok(crate::account_deletion::AccountDeletionBlockers {
            payments: row.get("payments"),
            payouts: row.get("payouts"),
            refunds: row.get("refunds"),
        })
    }
}
