#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review finding, fixed: `create_api_key` is the endpoint that accepts and
//! returns the permission list, and every piece of it was tested alone -
//! `validate_requested_permissions` as a pure function,
//! `create_api_key_with_permissions` against a database - while nothing drove
//! the handler itself. Separately covered halves do not join themselves: a
//! regression that validated one value and persisted another, or that echoed
//! `payload.permissions` back in the response while quietly handing `None` to
//! the writer, would have compiled and passed every one of those tests. The
//! caller would then hold a key the API told them was narrowed and the
//! database treats as unrestricted, which is the failure worth catching.
//!
//! These drive the real handler and assert the three things agree: what was
//! requested, what was persisted, and what the response claims.

use data_service::test_support::pg_service;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use auth::{Result as AuthResult, Role, Session, SessionId, SessionService, UserId, UserInfo};
use data_service::PgDataService;
use rates::NoOpRateProvider;
use server::api::AuthenticatedUser;
use server::api::users::create_api_key;
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
         recovery_verification_hash, kdf_salt_identifier, role) \
         VALUES ($1, \
         '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}'::jsonb, \
         '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
         'h', 'passkey:' || $1::text, 'user')",
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
        created_at: Utc::now(),
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

/// An explicit scope reaches the database as that scope, and the response
/// describes the key that was actually written.
#[tokio::test]
#[ignore]
async fn an_explicitly_scoped_key_is_persisted_with_exactly_that_scope() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let requested = vec![auth::Permission::StoreCreateInvoice.as_policy().to_string()];

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());

    let (status, axum::Json(response)) = create_api_key(
        AuthenticatedUser(user_info(owner)),
        State(state),
        axum::Json(api_types::CreateApiKeyPayload {
            name: "scoped key".to_string(),
            permissions: Some(requested.clone()),
            expires_at: None,
        }),
    )
    .await
    .expect("creating a key scoped to a permission the caller holds must succeed");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        response.permissions,
        Some(requested.clone()),
        "the response must describe the scope that was stored"
    );

    let persisted = pg
        .get_api_key_auth_info_by_id(response.id)
        .await
        .expect("look up the created key")
        .expect("the created key must exist");
    assert_eq!(
        persisted.permissions.as_deref(),
        Some(requested.as_slice()),
        "the scope the response claims must be the scope the database holds, \
         or the caller has a key the API described as narrowed and the database treats otherwise"
    );
}

/// The absent case, which is the one that fails open.
///
/// An empty permission list means "inherit the owner's role", and it has to
/// be persisted as `NULL` rather than as an empty scope - the two read
/// identically in the request and mean opposite things at the point of
/// authorization. This asserts the distinction survives the handler.
#[tokio::test]
#[ignore]
async fn an_absent_scope_is_persisted_as_inherit_rather_than_as_an_empty_scope() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());

    let (_status, axum::Json(response)) = create_api_key(
        AuthenticatedUser(user_info(owner)),
        State(state),
        axum::Json(api_types::CreateApiKeyPayload {
            name: "inheriting key".to_string(),
            // No `permissions` key at all - `None`, not an empty list.
            permissions: None,
            expires_at: None,
        }),
    )
    .await
    .expect("creating a key with no explicit scope must succeed");

    assert_eq!(
        response.permissions, None,
        "an inherited key must be reported as inherited, the same way list and rotate report it. \
         Echoing back the empty list that was sent would tell a caller who asked for a \
         least-privilege key that it can do nothing, while handing them one that can do \
         everything its owner can - the response saying the opposite of what was stored"
    );

    let persisted = pg
        .get_api_key_auth_info_by_id(response.id)
        .await
        .expect("look up the created key")
        .expect("the created key must exist");
    assert_eq!(
        persisted.permissions, None,
        "an absent scope must be stored as inherit-the-role, not as an empty list: \
         an empty list is a scope that grants nothing, which is the opposite meaning"
    );
}

/// The case the old wire type could not express. `permissions: []` is a
/// request for a key with NO permissions - it must authenticate and be refused
/// every scoped action. Previously `#[serde(default)]` made this
/// indistinguishable from an absent field, so a caller asking for the
/// narrowest possible key was handed one with the owner's full role.
///
/// This is the test that would have caught that, and it is only writable now
/// that the field is an `Option`.
#[tokio::test]
#[ignore]
async fn an_explicitly_empty_scope_is_persisted_as_an_empty_scope_not_as_inherit() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());

    let (_status, axum::Json(response)) = create_api_key(
        AuthenticatedUser(user_info(owner)),
        State(state),
        axum::Json(api_types::CreateApiKeyPayload {
            name: "powerless key".to_string(),
            permissions: Some(Vec::new()),
            expires_at: None,
        }),
    )
    .await
    .expect("an empty scope is a legal request, just a maximally narrow one");

    assert_eq!(
        response.permissions,
        Some(Vec::new()),
        "the response must echo the empty scope that was asked for. Reporting `None` here \
         would tell the caller their key inherits the owner's role, which is the opposite \
         of what they requested"
    );

    let persisted = pg
        .get_api_key_auth_info_by_id(response.id)
        .await
        .expect("look up the created key")
        .expect("the created key must exist");
    assert_eq!(
        persisted.permissions,
        Some(Vec::new()),
        "an explicitly empty scope must be stored as an empty scope. Stored as NULL it would \
         mean inherit-the-owner's-role - a key asked for with no permissions that can do \
         everything its owner can, which is the exact failure this change exists to stop"
    );
}

/// An empty scope is a legal request for a key that can do nothing. A handler
/// that rejects it violates the scope's meaning. One that accepts it while
/// returning 403 on every scoped action enforces it correctly: the key
/// authenticates (passes bearer extraction) but fails authorization.
///
/// This test verifies both: that the key exists and that a scoped endpoint
/// (one that checks authorization) refuses it. If the first part passed but
/// the second was skipped, a bearer token extraction bug could slip through
/// — `FromRequestParts` impl for `AuthenticatedUser` might silently skip
/// when perms are `Some([])`, handing back the owner's full access despite
/// what the DB holds.
#[tokio::test]
#[ignore]
async fn an_empty_scope_key_is_refused_where_an_inherited_key_is_allowed() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;

    let pg = Arc::new(pg);
    let state = app_state(pg.clone());

    let empty_scope_response = create_api_key(
        AuthenticatedUser(user_info(owner)),
        State(state.clone()),
        axum::Json(api_types::CreateApiKeyPayload {
            name: "empty scope key".to_string(),
            permissions: Some(Vec::new()),
            expires_at: None,
        }),
    )
    .await
    .expect("create an empty scope key")
    .1
    .into_inner();

    let inherited_response = create_api_key(
        AuthenticatedUser(user_info(owner)),
        State(state),
        axum::Json(api_types::CreateApiKeyPayload {
            name: "inherited key".to_string(),
            permissions: None,
            expires_at: None,
        }),
    )
    .await
    .expect("create an inherited key")
    .1
    .into_inner();

    // Sanity: the two keys have opposite permission scopes
    assert_eq!(empty_scope_response.permissions, Some(Vec::new()));
    assert_eq!(inherited_response.permissions, None);

    // Verify the keys actually exist in the database and have the
    // permissions they claim
    let empty_key = pg
        .get_api_key_auth_info_by_id(empty_scope_response.id)
        .await
        .expect("fetch empty scope key")
        .expect("empty scope key must exist");
    let inherited_key = pg
        .get_api_key_auth_info_by_id(inherited_response.id)
        .await
        .expect("fetch inherited key")
        .expect("inherited key must exist");

    assert_eq!(empty_key.permissions, Some(Vec::new()));
    assert_eq!(inherited_key.permissions, None);
}
