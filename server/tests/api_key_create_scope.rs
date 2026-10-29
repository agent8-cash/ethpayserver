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
//! `CreateApiKeyPayload.permissions` is `Option<Vec<String>>`: an absent
//! field means "inherit the owner's role" (`None`), an explicit `[]` means
//! "a deliberately powerless key" (`Some(vec![])`), and a non-empty array
//! scopes the key to those policies. The first group of tests below drives
//! the real handler for all three and asserts what was requested, what was
//! persisted, and what the response claims, all agree.
//!
//! The second group goes further, through the real router with a real
//! `Authorization: Bearer` key: it creates a key via `POST /users/api-keys`
//! and then uses the *returned* key against a real scoped endpoint, so the
//! "honoured" half of the empty-scope contract - the key authenticates and
//! is refused by permission, not by authentication - is exercised the same
//! way a caller would see it, not asserted only against the stored row.

use data_service::test_support::pg_service;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use auth::{
    AuthConfig, AuthService, Result as AuthResult, Role, Session, SessionId, SessionService, Store,
    UserId, UserInfo,
};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
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
/// An omitted `permissions` field means "inherit the owner's role", and it
/// has to be persisted as `NULL` rather than as an empty scope - the two
/// used to be indistinguishable on the wire and are opposites at the point
/// of authorization. This asserts the distinction survives the handler.
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
            permissions: None,
            expires_at: None,
        }),
    )
    .await
    .expect("creating a key with no explicit scope must succeed");

    assert_eq!(
        response.permissions, None,
        "an inherited key must be reported as inherited, the same way list and rotate report it. \
         Echoing back an empty list would tell a caller who asked for a least-privilege key that \
         it can do nothing, while handing them one that can do everything its owner can - the \
         response saying the opposite of what was stored"
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

/// The explicit-empty case: distinct from both of the above. A caller who
/// deliberately sends `permissions: []` gets a key that authenticates and can
/// do nothing, and the response has to say so - `[]`, not `null`.
#[tokio::test]
#[ignore]
async fn an_explicitly_empty_scope_is_persisted_as_empty_not_as_inherit() {
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
    .expect("creating a deliberately empty-scope key must succeed");

    assert_eq!(
        response.permissions,
        Some(Vec::new()),
        "an explicitly empty scope must be reported as an empty list, not as inherited"
    );

    let persisted = pg
        .get_api_key_auth_info_by_id(response.id)
        .await
        .expect("look up the created key")
        .expect("the created key must exist");
    assert_eq!(
        persisted.permissions,
        Some(Vec::new()),
        "an explicitly empty scope must be stored as an empty list (not NULL), or it would \
         authenticate as inheriting the owner's role instead of granting nothing"
    );
}

// --- Through the real router, with the real returned key -------------------
//
// The tests above prove the stored row matches the request and the response.
// They do not prove the key the response handed back actually behaves that
// way once used: that needs the real auth extractor, which lives behind
// `POST /users/api-keys` and whatever endpoint the new key is pointed at
// next, not behind a direct call into `create_api_key`.

async fn pg_app(pg: &Arc<PgDataService>) -> axum::Router {
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(pg),
        AuthConfig::default(),
    ));
    let state = PgAppState::new(
        Arc::clone(pg),
        auth_service,
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    server::api::router(state, false, None, None, None)
}

/// A user, an unscoped API key for them to act as the caller with, and a
/// store they own - everything `POST /users/api-keys` plus a store-scoped
/// `GET /invoices` needs, seeded directly so only the endpoints under test
/// run through the router.
struct Caller {
    key: String,
    store_id: Uuid,
}

async fn seed_caller(pg: &PgDataService) -> Caller {
    let user_id = seed_user(pg.pool()).await;

    let key = format!("ak_create_scope_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix) \
         VALUES ($1, $2, 'caller key', $3, 'ak_test')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pg.pool())
    .await
    .expect("seed caller api key");

    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(user_id));
    pg.create_store_owned_by(&store, UserId(user_id))
        .await
        .expect("seed store owned by caller");

    Caller {
        key,
        store_id: store.id.0,
    }
}

async fn post_json(app: &axum::Router, key: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    let resp = app.clone().oneshot(req).await.expect("router call");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(app: &axum::Router, key: &str, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .expect("build request");
    app.clone()
        .oneshot(req)
        .await
        .expect("router call")
        .status()
}

/// The pair the ticket asks for: a key created with `permissions: []` through
/// the real `POST /users/api-keys` authenticates - it is not rejected the way
/// a revoked or unknown key would be - and is then refused on a store-scoped
/// endpoint (403, not 401). A key created with no `permissions` field at all
/// is allowed on the same endpoint, proving the refusal above is about scope,
/// not about something broken in the new-key path generally.
#[tokio::test]
#[ignore]
async fn an_empty_scope_key_is_refused_where_an_inherited_key_is_allowed() {
    let pg = Arc::new(pg_service().await);
    let caller = seed_caller(&pg).await;
    let app = pg_app(&pg).await;

    let (status, inherited) = post_json(
        &app,
        &caller.key,
        "/users/api-keys",
        json!({ "name": "inherited" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{inherited:?}");
    assert_eq!(inherited["permissions"], Value::Null);
    let inherited_key = inherited["key"].as_str().expect("key field").to_string();

    let (status, empty) = post_json(
        &app,
        &caller.key,
        "/users/api-keys",
        json!({ "name": "powerless", "permissions": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{empty:?}");
    assert_eq!(empty["permissions"], json!([]));
    let empty_key = empty["key"].as_str().expect("key field").to_string();

    let invoices_uri = format!("/invoices?store_id={}", caller.store_id);

    assert_eq!(
        get(&app, &inherited_key, &invoices_uri).await,
        StatusCode::OK,
        "a key with no requested scope must inherit the owner's role and be let through"
    );

    assert_eq!(
        get(&app, &empty_key, &invoices_uri).await,
        StatusCode::FORBIDDEN,
        "a key explicitly created with permissions: [] authenticates (it is a real, active \
         key - a bad key would 401) but must be refused every scoped action, including one \
         its owner could otherwise reach"
    );
}
