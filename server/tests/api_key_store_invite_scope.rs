#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `create_store_invite` (stores/invites.rs) inlines its own
//! `key_grants_store_permission` check the same way `list_store_members` and
//! `get_store_wallet` did before `api_key_inline_scope_checks.rs` drove them
//! with a real scoped key. Nothing did the same for the invite handler, so a
//! copy-paste slip in its own `&& key_grants_store_permission(..)` clause -
//! or dropping the clause outright - would compile and pass the whole suite.
//! These two tests drive it with a real `Some(scope)` key in both directions.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use auth::{
    Policies, Result as AuthResult, Role, Session, SessionId, SessionService, Store, UserId,
    UserInfo,
};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
use data_service::test_support::pg_service;
use rates::NoOpRateProvider;
use server::api::StoreScopedUser;
use server::api::stores::{CreateInviteRequest, create_store_invite};
use server::services::RedisEVMMonitor;
use server::state::PgAppState;

struct UnusedSessionService;

#[async_trait]
impl SessionService for UnusedSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by this handler")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by this handler")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by this handler")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by this handler")
    }
}

async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text)",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

fn user_info(id: Uuid) -> UserInfo {
    UserInfo {
        id: UserId(id),
        email: None,
        primary_wallet_address: None,
        created_at: chrono::Utc::now(),
        last_login_at: None,
        role: Role::User,
    }
}

fn app_state(data_service: Arc<PgDataService>) -> PgAppState<UnusedSessionService> {
    PgAppState::new(
        data_service,
        Arc::new(UnusedSessionService),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    )
}

/// A key scoped to `caninviteusers` reaches past `create_store_invite`'s own
/// inline permission check. It still fails downstream - email is not
/// configured on this state (`NoopEmailSender`), which the handler checks
/// right after validating the request body - so the point is *which* check
/// it fails, not full success.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_can_invite_users_reaches_past_the_permission_check() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user, with the Owner role's caninviteusers permission");

    let state = app_state(Arc::new(pg));

    let result = create_store_invite(
        StoreScopedUser(
            user_info(owner),
            Some(vec!["ethpay.store.caninviteusers".to_string()]),
        ),
        State(state),
        Path(store.id.0),
        Json(CreateInviteRequest {
            email: "colleague@example.com".to_string(),
            role: None,
        }),
    )
    .await;

    assert_eq!(
        result,
        Err(StatusCode::SERVICE_UNAVAILABLE),
        "a key scoped to caninviteusers must pass create_store_invite's permission check \
         and fail only on email being unconfigured"
    );
}

/// The other direction: the owner has `caninviteusers` (via the default
/// Owner role), but the key is scoped to something else - `create_store_invite`
/// must refuse it.
#[tokio::test]
#[ignore]
async fn a_key_scoped_to_something_else_is_refused_create_invite() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");

    let state = app_state(Arc::new(pg));

    let result = create_store_invite(
        StoreScopedUser(
            user_info(owner),
            Some(vec![Policies::STORE_CREATE_INVOICE.to_string()]),
        ),
        State(state),
        Path(store.id.0),
        Json(CreateInviteRequest {
            email: "colleague@example.com".to_string(),
            role: None,
        }),
    )
    .await;

    assert_eq!(
        result,
        Err(StatusCode::FORBIDDEN),
        "a key not scoped to caninviteusers must be refused by create_store_invite"
    );
}
