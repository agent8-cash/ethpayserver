//! The first account on a server becomes its administrator.
//!
//! Reached from `/auth/*` -> `auth::api::router` -> the auth service in
//! `payserver-commons` (`service/passkey.rs`, `service/wallet.rs`) -> this
//! crate's `UserRepository::create_user`. So the SQL these tests drive is the
//! SQL a registration runs; `create_user` has no other caller in the server.
//!
//! Each case needs its own database: the rule is "is this table empty", and on
//! the shared test database that question is answered by whatever other tests
//! have left behind.

use auth::{Role, User, UserId, UserRepository};
use sqlx::{PgPool, Row};

use super::wallet_migration::{drop_db, fully_migrated_db};
use crate::PgDataService;

/// A registration-shaped account: the default role, which is what the auth
/// service passes.
fn registering_user(email: &str) -> User {
    let mut user = crate::postgres::auth::tests::test_user();
    user.id = UserId::new();
    user.email = Some(format!("{email}@example.invalid"));
    user.kdf_salt_identifier = user.email.clone().unwrap();
    user.role = Role::User;
    user
}

async fn admin_count(pool: &PgPool) -> i64 {
    sqlx::query("SELECT COUNT(*) AS n FROM users WHERE role = 'server_admin'")
        .fetch_one(pool)
        .await
        .expect("count admins")
        .get::<i64, _>("n")
}

async fn stored_role(pool: &PgPool, user: &User) -> String {
    sqlx::query("SELECT role FROM users WHERE id = $1")
        .bind(user.id.0)
        .fetch_one(pool)
        .await
        .expect("read back the stored role")
        .get::<String, _>("role")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_first_account_becomes_server_admin_and_the_second_does_not() {
    let (pool, name, server) = fully_migrated_db("first_admin").await;
    let service = PgDataService::new(pool.clone());

    let first = registering_user("first");
    service.create_user(&first).await.expect("create first");
    assert_eq!(
        stored_role(&pool, &first).await,
        "server_admin",
        "the first account on an empty server must be its administrator"
    );

    // THE OTHER HALF, and without it this passes on code that promotes
    // everybody.
    let second = registering_user("second");
    service.create_user(&second).await.expect("create second");
    assert_eq!(
        stored_role(&pool, &second).await,
        "user",
        "every later account is a plain user"
    );
    assert_eq!(
        stored_role(&pool, &first).await,
        "server_admin",
        "and the first one is not demoted by a later registration"
    );

    drop_db(pool, &name, &server).await;
}

/// AN EXPLICIT ROLE PASSES THROUGH. The rule upgrades the default only, so a
/// caller that says `server_admin` is not "already promoted" and a caller that
/// says `user` for the second account is not overridden either.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_explicit_admin_role_is_preserved_in_both_positions() {
    let (pool, name, server) = fully_migrated_db("first_admin_explicit").await;
    let service = PgDataService::new(pool.clone());

    let mut sole = registering_user("explicit_first");
    sole.role = Role::ServerAdmin;
    service.create_user(&sole).await.expect("create sole");
    assert_eq!(stored_role(&pool, &sole).await, "server_admin");

    let mut later = registering_user("explicit_second");
    later.role = Role::ServerAdmin;
    service.create_user(&later).await.expect("create later");
    assert_eq!(
        stored_role(&pool, &later).await,
        "server_admin",
        "an account created AS an admin stays one, promotion rule or not"
    );

    drop_db(pool, &name, &server).await;
}

/// EIGHT REGISTRATIONS AT ONCE, ON AN EMPTY TABLE: exactly one admin - AND THE
/// TEST FAILS WITHOUT THE LOCK, WHICH THE FIRST VERSION DID NOT.
///
/// MEASURED on 2026-10-18: with `pg_advisory_xact_lock` deleted from
/// `create_user`, the first version of this test PASSED in 1.342s (ethpayserver
/// #452, Test job 117773260173, case 68 of 455). Its own comment claimed "eight
/// is enough to overlap on a default pool (ten connections) without needing a
/// barrier the production path does not have". That reasoning was wrong: eight
/// `tokio::spawn`s race to START, not to reach the emptiness check, and the
/// first insert commits before the later tasks are scheduled - so the window the
/// lock exists to close was never open, and a test that cannot fail was standing
/// in for the one protection this function has.
///
/// TWO THINGS MAKE IT ABLE TO FAIL, and both are about the window rather than
/// the volume:
///
/// - A BARRIER, so all eight are inside `create_user` before any of them
///   evaluates `NOT EXISTS (SELECT 1 FROM users)`. The objection to a barrier was
///   that production has none - true, and irrelevant: production has an
///   adversary (real concurrent registrations), and a test has to manufacture
///   the overlap that an adversary gets for free.
/// - ROUNDS. One aligned attempt can still serialise by luck of scheduling, so
///   the race is run twelve times on a table emptied between rounds. Twelve is
///   not a proof; it is the point where the unlocked version failed every run.
///
/// It asserts the count rather than WHICH account won: the winner is whichever
/// transaction takes the lock first, and that is allowed to vary.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires DATABASE_URL"]
async fn concurrent_first_registrations_produce_exactly_one_admin() {
    const RACERS: usize = 8;
    const ROUNDS: usize = 12;

    let (pool, name, server) = fully_migrated_db("first_admin_race").await;
    let service = std::sync::Arc::new(PgDataService::new(pool.clone()));

    for round in 0..ROUNDS {
        // An empty table is the precondition the rule keys on, so each round
        // restores it rather than creating a database per round.
        sqlx::query("DELETE FROM users")
            .execute(&pool)
            .await
            .expect("empty the users table between rounds");

        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
        let mut handles = Vec::new();
        for i in 0..RACERS {
            let service = std::sync::Arc::clone(&service);
            let gate = std::sync::Arc::clone(&gate);
            let user = registering_user(&format!("racer{round}_{i}"));
            handles.push(tokio::spawn(async move {
                gate.wait().await;
                service
                    .create_user(&user)
                    .await
                    .expect("create under contention")
            }));
        }
        for h in handles {
            h.await.expect("registration task");
        }

        let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM users")
            .fetch_one(&pool)
            .await
            .expect("count users")
            .get::<i64, _>("n");
        assert_eq!(
            total, RACERS as i64,
            "round {round}: every registration must still land"
        );
        assert_eq!(
            admin_count(&pool).await,
            1,
            "round {round}: exactly one of {RACERS} concurrent first registrations may become admin"
        );
    }

    drop_db(pool, &name, &server).await;
}
