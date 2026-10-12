#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A store owner brings a colleague in by invite, and the invite reveals
//! nothing about who has an account.
//!
//! Drives the real router with real `Authorization: Bearer` keys, so routes,
//! extractors, permissions and handlers are all on the path. The outcomes are
//! what is asserted - the response bytes, the membership rows, what the
//! invited account can then read - because a status code alone cannot tell an
//! endpoint that makes a known address a member immediately from one that
//! waits for consent.
//!
//! Needs `DATABASE_URL`; skips when unset, like the other ignored integration
//! tests, and runs in CI's `--run-ignored` step.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

use auth::{AuthConfig, AuthService, Store, UserId};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
use data_service::test_support::pg_service;
use rates::NoOpRateProvider;
use server::services::RedisEVMMonitor;
use server::services::email::{
    AccountNotice, EmailChangeVerificationData, EmailError, EmailSender, ReceiptData,
};
use server::state::PgAppState;

/// Records every notice instead of sending it, so a test can read the code
/// the way the invited address owner would.
#[derive(Default)]
struct Outbox(Mutex<Vec<(String, AccountNotice)>>);

impl Outbox {
    /// The code mailed to `address`, if anything was.
    fn code_for(&self, address: &str) -> Option<Uuid> {
        let sent = self.0.lock().unwrap();
        let (_, notice) = sent
            .iter()
            .rev()
            .find(|(to, _)| to.eq_ignore_ascii_case(address))?;
        notice
            .body
            .split_whitespace()
            .find_map(|w| Uuid::parse_str(w).ok())
    }
}

#[async_trait]
impl EmailSender for Outbox {
    async fn send_receipt(&self, _: &str, _: &ReceiptData) -> Result<(), EmailError> {
        Ok(())
    }
    async fn send_email_change_verification(
        &self,
        _: &str,
        _: &EmailChangeVerificationData,
    ) -> Result<(), EmailError> {
        Ok(())
    }
    async fn send_account_notice(
        &self,
        to: &str,
        notice: &AccountNotice,
    ) -> Result<(), EmailError> {
        self.0
            .lock()
            .unwrap()
            .push((to.to_string(), notice.clone()));
        Ok(())
    }
    fn is_configured(&self) -> bool {
        true
    }
}

struct Account {
    id: Uuid,
    key: String,
}

/// A user (with `email`, if given) and an API key for them.
async fn seed_account(pg: &PgDataService, email: Option<&str>) -> Account {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, email, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, $2, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"\"}'::jsonb, \
             '{\"ciphertext\":\"\",\"iv\":\"\",\"mac\":\"\"}'::jsonb, \
             'h', 'passkey:' || $1::text)",
    )
    .bind(id)
    .bind(email)
    .execute(pg.pool())
    .await
    .expect("seed user");

    let key = format!("ak_test_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix) \
         VALUES ($1, $2, 'invite test key', $3, 'ak_test')",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pg.pool())
    .await
    .expect("seed api key");
    Account { id, key }
}

async fn seed_store(pg: &PgDataService, owner: &Account) -> Store {
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner.id));
    pg.create_store_owned_by(&store, UserId(owner.id))
        .await
        .expect("seed store");
    store
}

fn app(pg: &Arc<PgDataService>, outbox: &Arc<Outbox>) -> Router {
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(pg),
        AuthConfig::default(),
    ));
    let state = PgAppState::new(
        Arc::clone(pg),
        auth_service,
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::clone(outbox) as Arc<dyn EmailSender>,
    );
    server::api::router(state, false, None, None, None)
}

async fn call(
    app: &Router,
    key: &str,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    let body = match body {
        Some(json) => {
            request = request.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("build request"))
        .await
        .expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, bytes.to_vec())
}

async fn invite(app: &Router, key: &str, store: &Store, email: &str) -> (StatusCode, Vec<u8>) {
    call(
        app,
        key,
        Method::POST,
        &format!("/stores/{}/invites", store.id.0),
        Some(serde_json::json!({ "email": email })),
    )
    .await
}

async fn accept(app: &Router, key: &str, code: Uuid) -> (StatusCode, Vec<u8>) {
    call(
        app,
        key,
        Method::POST,
        "/users/me/invites/accept",
        Some(serde_json::json!({ "token": code })),
    )
    .await
}

async fn is_member(pg: &PgDataService, user: Uuid, store: &Store) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_stores WHERE user_id = $1 AND store_id = $2)",
    )
    .bind(user)
    .bind(store.id.0)
    .fetch_one(pg.pool())
    .await
    .expect("membership query")
}

#[tokio::test]
#[ignore]
async fn the_response_is_identical_for_a_known_and_an_unknown_address_and_nobody_joins_early() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let known_email = format!("known-{}@example.com", Uuid::new_v4());
    let unknown_email = format!("unknown-{}@example.com", Uuid::new_v4());
    let owner = seed_account(&pg, None).await;
    let colleague = seed_account(&pg, Some(&known_email)).await;
    let store = seed_store(&pg, &owner).await;

    let (known_status, known_body) = invite(&app, &owner.key, &store, &known_email).await;
    let (unknown_status, unknown_body) = invite(&app, &owner.key, &store, &unknown_email).await;

    assert_eq!(known_status, StatusCode::ACCEPTED);
    assert_eq!(
        (known_status, &known_body),
        (unknown_status, &unknown_body),
        "the requester must not be able to tell a registered address from an unregistered one"
    );

    // The effect, not the status: the registered address must not have been
    // made a member behind the 202, and the member list must look the same
    // either way.
    assert!(
        !is_member(&pg, colleague.id, &store).await,
        "an invite must not create a membership before the address owner accepts"
    );
    let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_stores WHERE store_id = $1")
        .bind(store.id.0)
        .fetch_one(pg.pool())
        .await
        .unwrap();
    assert_eq!(members, 1, "only the owner is a member before acceptance");

    // Both addresses were told, so the delivery path did not branch either.
    assert!(outbox.code_for(&known_email).is_some());
    assert!(outbox.code_for(&unknown_email).is_some());
}

#[tokio::test]
#[ignore]
async fn after_acceptance_the_member_can_list_the_stores_invoices() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let email = format!("colleague-{}@example.com", Uuid::new_v4());
    let owner = seed_account(&pg, None).await;
    let colleague = seed_account(&pg, Some(&email)).await;
    let store = seed_store(&pg, &owner).await;
    let list = format!("/invoices?store_id={}", store.id.0);

    // Control: before acceptance the colleague is refused, so the 200 below
    // is the membership and not a list that was never gated.
    let (before, _) = call(&app, &colleague.key, Method::GET, &list, None).await;
    assert_ne!(
        before,
        StatusCode::OK,
        "a non-member must not list the store"
    );

    assert_eq!(
        invite(&app, &owner.key, &store, &email).await.0,
        StatusCode::ACCEPTED
    );
    let code = outbox.code_for(&email).expect("the invite was mailed");
    let (status, body) = accept(&app, &colleague.key, code).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(is_member(&pg, colleague.id, &store).await);

    let (after, body) = call(&app, &colleague.key, Method::GET, &list, None).await;
    assert_eq!(after, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // A code works once.
    let (again, _) = accept(&app, &colleague.key, code).await;
    assert_eq!(again, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore]
async fn a_guest_cannot_invite_and_a_stranger_cannot_either() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let email = format!("guest-{}@example.com", Uuid::new_v4());
    let owner = seed_account(&pg, None).await;
    let guest = seed_account(&pg, Some(&email)).await;
    let stranger = seed_account(&pg, None).await;
    let store = seed_store(&pg, &owner).await;

    // Control: the owner's identical request succeeds.
    assert_eq!(
        invite(&app, &owner.key, &store, &email).await.0,
        StatusCode::ACCEPTED
    );
    let code = outbox.code_for(&email).unwrap();
    assert_eq!(accept(&app, &guest.key, code).await.0, StatusCode::OK);

    let sent_before = outbox.0.lock().unwrap().len();
    let target = format!("third-{}@example.com", Uuid::new_v4());
    assert_eq!(
        invite(&app, &guest.key, &store, &target).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        invite(&app, &stranger.key, &store, &target).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        outbox.0.lock().unwrap().len(),
        sent_before,
        "a refused invite must send nothing"
    );
}

#[tokio::test]
#[ignore]
async fn an_invite_cannot_demote_an_existing_member_or_grant_ownership() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let email = format!("owner2-{}@example.com", Uuid::new_v4());
    let owner = seed_account(&pg, Some(&email)).await;
    let store = seed_store(&pg, &owner).await;

    // The owner invites their own address: accepting must not replace Owner
    // with the invite's role.
    assert_eq!(
        invite(&app, &owner.key, &store, &email).await.0,
        StatusCode::ACCEPTED
    );
    let code = outbox.code_for(&email).unwrap();
    assert_eq!(accept(&app, &owner.key, code).await.0, StatusCode::CONFLICT);
    let role: String = sqlx::query_scalar(
        "SELECT sr.role FROM user_stores us JOIN store_roles sr ON sr.id = us.store_role_id \
         WHERE us.user_id = $1 AND us.store_id = $2",
    )
    .bind(owner.id)
    .bind(store.id.0)
    .fetch_one(pg.pool())
    .await
    .unwrap();
    assert_eq!(role, "Owner");

    let (status, _) = call(
        &app,
        &owner.key,
        Method::POST,
        &format!("/stores/{}/invites", store.id.0),
        Some(serde_json::json!({ "email": "x@example.com", "role": "Owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Invites `email` at `role` and redeems the mailed code as `account`.
async fn join_as(
    app: &Router,
    outbox: &Outbox,
    owner: &Account,
    store: &Store,
    account: &Account,
    email: &str,
    role: &str,
) {
    let (status, _) = call(
        app,
        &owner.key,
        Method::POST,
        &format!("/stores/{}/invites", store.id.0),
        Some(serde_json::json!({ "email": email, "role": role })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let code = outbox.code_for(email).expect("the invite was mailed");
    let (status, body) = accept(app, &account.key, code).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
#[ignore]
async fn a_reinvite_revokes_the_earlier_code() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let email = format!("reinvite-{}@example.com", Uuid::new_v4());
    let owner = seed_account(&pg, None).await;
    let colleague = seed_account(&pg, Some(&email)).await;
    let store = seed_store(&pg, &owner).await;

    assert_eq!(
        invite(&app, &owner.key, &store, &email).await.0,
        StatusCode::ACCEPTED
    );
    let first = outbox.code_for(&email).unwrap();
    assert_eq!(
        invite(&app, &owner.key, &store, &email).await.0,
        StatusCode::ACCEPTED
    );
    let second = outbox.code_for(&email).unwrap();
    assert_ne!(first, second, "a re-invite must mint a new code");

    let (status, _) = accept(&app, &colleague.key, first).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a replaced code must stop working"
    );
    assert!(
        !is_member(&pg, colleague.id, &store).await,
        "a replaced code must not create a membership"
    );

    let (status, body) = accept(&app, &colleague.key, second).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(is_member(&pg, colleague.id, &store).await);
}

#[tokio::test]
#[ignore]
async fn an_expired_invite_is_refused() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let email = format!("expired-{}@example.com", Uuid::new_v4());
    let owner = seed_account(&pg, None).await;
    let colleague = seed_account(&pg, Some(&email)).await;
    let store = seed_store(&pg, &owner).await;

    assert_eq!(
        invite(&app, &owner.key, &store, &email).await.0,
        StatusCode::ACCEPTED
    );
    let code = outbox.code_for(&email).unwrap();
    let moved = sqlx::query(
        "UPDATE store_invites SET expires_at = NOW() - INTERVAL '1 minute' WHERE token = $1",
    )
    .bind(code)
    .execute(pg.pool())
    .await
    .unwrap();
    assert_eq!(moved.rows_affected(), 1, "the invite row must exist");

    let (status, _) = accept(&app, &colleague.key, code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !is_member(&pg, colleague.id, &store).await,
        "an expired code must not create a membership"
    );
}

#[tokio::test]
#[ignore]
async fn a_manager_and_an_employee_cannot_invite() {
    let pg = pg_service().await;
    let pg = Arc::new(pg);
    let outbox = Arc::new(Outbox::default());
    let app = app(&pg, &outbox);

    let owner = seed_account(&pg, None).await;
    let store = seed_store(&pg, &owner).await;

    for role in ["Manager", "Employee"] {
        let email = format!("{role}-{}@example.com", Uuid::new_v4());
        let member = seed_account(&pg, Some(&email)).await;
        join_as(&app, &outbox, &owner, &store, &member, &email, role).await;

        // Control: the owner's identical request succeeds.
        let target = format!("target-{}@example.com", Uuid::new_v4());
        assert_eq!(
            invite(&app, &owner.key, &store, &target).await.0,
            StatusCode::ACCEPTED
        );
        assert!(outbox.code_for(&target).is_some());

        let refused = format!("refused-{}@example.com", Uuid::new_v4());
        assert_eq!(
            invite(&app, &member.key, &store, &refused).await.0,
            StatusCode::FORBIDDEN,
            "{role} must not invite"
        );
        assert!(
            outbox.code_for(&refused).is_none(),
            "a refused invite from {role} must send nothing"
        );
    }
}
