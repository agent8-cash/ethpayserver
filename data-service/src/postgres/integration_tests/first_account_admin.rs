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
