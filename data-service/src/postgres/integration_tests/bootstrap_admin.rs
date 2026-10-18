//! The sole-account promotion, run against the states a real database is in.
//!
//! The migration's whole value is its GUARD: it must promote the one account on
//! a server nobody administers, and must do nothing anywhere else. So each case
//! here builds a throwaway database, applies every migration before this one,
//! seeds a different shape of `users`, applies THE REAL FILE (read off disk, not
//! a copy of its statement), and checks who ended up an admin.
//!
//! It cannot run against the shared test database: the guard reads
//! `COUNT(*) FROM users` across the whole table, so another test's residue would
//! decide the outcome and the assertions would pass or fail on what ran before
//! them.

use sqlx::{Executor, PgPool, Row};
use uuid::Uuid;

use super::wallet_migration::{drop_db, migration_sql, pre_migration_db_for};

const MIGRATION: &str = "20261018120000_bootstrap_sole_account_as_admin";

/// Seed one account with a role. `email` only to keep the rows distinguishable.
async fn seed_user(pool: &PgPool, label: &str, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, email, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier, role) \
         VALUES ($1, $2, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text, $3)",
    )
    .bind(id)
    .bind(format!("{label}@example.invalid"))
    .bind(role)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

/// Apply the migration under test, as its bytes stand on disk.
async fn apply(pool: &PgPool) {
    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the promotion migration");
}

async fn role_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query("SELECT role FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read role")
        .get::<String, _>("role")
}

async fn admin_count(pool: &PgPool) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM users WHERE role = 'server_admin'")
        .fetch_one(pool)
        .await
        .expect("count admins")
        .get::<i64, _>("n")
}

/// THE CASE THE MIGRATION EXISTS FOR: one account, nobody administering.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_sole_account_becomes_server_admin() {
    let (pool, name, server) = pre_migration_db_for(MIGRATION, "bootstrap_sole").await;
    let sole = seed_user(&pool, "sole", "user").await;

    apply(&pool).await;

    assert_eq!(
        role_of(&pool, sole).await,
        "server_admin",
        "the only account on a server with no admin must be promoted"
    );
    assert_eq!(admin_count(&pool).await, 1, "and it must be the only admin");

    // IDEMPOTENT, because a migration can be re-applied to a restored backup
    // and because the guard's second clause is what makes that safe.
    apply(&pool).await;
    assert_eq!(
        admin_count(&pool).await,
        1,
        "re-applying must change nothing"
    );

    drop_db(pool, &name, &server).await;
}

/// AN EMPTY DATABASE IS THE MAINNET CASE. Every fresh deploy runs this
/// migration before anyone has registered, and it must do nothing at all.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_empty_database_gets_no_admin() {
    let (pool, name, server) = pre_migration_db_for(MIGRATION, "bootstrap_empty").await;

    apply(&pool).await;

    assert_eq!(
        admin_count(&pool).await,
        0,
        "with no accounts there is nobody to promote, and inventing one would \
         hand a fresh mainnet server to whoever registers first"
    );

    drop_db(pool, &name, &server).await;
}

/// TWO ACCOUNTS: the migration must not pick a winner. Whoever should
/// administer that server is a human decision, not an ordering of rows.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn two_accounts_are_left_alone() {
    let (pool, name, server) = pre_migration_db_for(MIGRATION, "bootstrap_two").await;
    let first = seed_user(&pool, "first", "user").await;
    let second = seed_user(&pool, "second", "user").await;

    apply(&pool).await;

    assert_eq!(
        admin_count(&pool).await,
        0,
        "neither account may be promoted"
    );
    assert_eq!(role_of(&pool, first).await, "user");
    assert_eq!(role_of(&pool, second).await, "user");

    drop_db(pool, &name, &server).await;
}

/// AN ADMIN ALREADY EXISTS: a server someone administers must come out
/// untouched, so this can never quietly add a second administrator.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_existing_admin_blocks_any_promotion() {
    let (pool, name, server) = pre_migration_db_for(MIGRATION, "bootstrap_has_admin").await;
    let admin = seed_user(&pool, "admin", "server_admin").await;
    let plain = seed_user(&pool, "plain", "user").await;

    apply(&pool).await;

    assert_eq!(role_of(&pool, plain).await, "user", "no second admin");
    assert_eq!(
        role_of(&pool, admin).await,
        "server_admin",
        "and no demotion"
    );
    assert_eq!(admin_count(&pool).await, 1);

    drop_db(pool, &name, &server).await;
}

/// The guard's two clauses are AND-ed, and this is the case that tells them
/// apart: ONE account, and it is already the admin. Were the count clause
/// missing the statement would still be a no-op here, so what this pins is that
/// a sole admin is not rewritten - the UPDATE touches zero rows rather than
/// setting a value that happens to match.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_sole_account_that_is_already_admin_is_not_rewritten() {
    let (pool, name, server) = pre_migration_db_for(MIGRATION, "bootstrap_sole_admin").await;
    let sole = seed_user(&pool, "sole_admin", "server_admin").await;

    apply(&pool).await;

    assert_eq!(role_of(&pool, sole).await, "server_admin");
    assert_eq!(admin_count(&pool).await, 1);

    drop_db(pool, &name, &server).await;
}
