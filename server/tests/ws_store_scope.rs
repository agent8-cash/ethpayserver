#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `/ws` delivers an invoice or payment update only to a socket whose user
//! may see the store it happened in: a server admin, or a member of that
//! store.
//!
//! These tests go through the real `/ws` handler - upgrade, auth frame,
//! session validation, forwarding loop - over a TCP socket, with updates
//! published through the same `WsBroadcast` the event consumers use. Absence
//! is only ever asserted after a positive control: a channel that is simply
//! not delivering would otherwise pass every "does not receive" check.
//!
//! Updates are routed to per-store channels, so a socket is never subscribed
//! to a store its user cannot see. "The first frame is the tenant's own
//! update" proves the other tenant's update, published before it, was not
//! delivered rather than merely late.
//!
//! Routing is not the access decision. That is taken per event, immediately
//! before the send, so the tests that pin revocation run with the periodic
//! re-check set to an hour: with no tick possible, only a decision taken on
//! the event itself can withhold or close.
//!
//! And a client cannot keep access by refusing to read: a send that a peer
//! will not accept within the deadline closes the socket.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use auth::{
    Result as AuthResult, Session, SessionId, SessionService, Store, UserId, UserInfo, UserStore,
    UserStoreRepository,
};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
use rates::NoOpRateProvider;
use server::api::ws::{DEFAULT_SEND_DEADLINE, StatusUpdate, WsBroadcast, ws_handler};
use server::services::RedisEVMMonitor;
use server::state::PgAppState;

#[allow(dead_code)]
#[path = "cross_tenant_isolation/support.rs"]
mod support;

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(600);

/// Maps a session token to the user it was issued to. The `/ws` handler only
/// asks it who a token belongs to; everything else it decides from the
/// database, which is what these tests are about.
struct Sessions(std::sync::Mutex<HashMap<SessionId, UserInfo>>);

impl Sessions {
    fn revoke(&self, session: SessionId) {
        self.0.lock().unwrap().remove(&session);
    }
}

#[async_trait]
impl SessionService for Sessions {
    async fn validate_session(&self, session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        match self.0.lock().unwrap().get(&session_id).cloned() {
            Some(user) => Ok((user.clone(), Session::new(user.id, auth::DeviceId::new()))),
            None => Err(auth::AuthError::SessionInvalid),
        }
    }
    async fn logout(&self, _: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised")
    }
    async fn logout_all(&self, _: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised")
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Person {
    user_id: Uuid,
    session: SessionId,
}

impl Person {
    async fn new(pool: &PgPool) -> Self {
        Self {
            user_id: support::seed_user(pool).await,
            session: SessionId(Uuid::new_v4()),
        }
    }
}

struct Harness {
    addr: SocketAddr,
    broadcast: Arc<WsBroadcast>,
    sessions: Arc<Sessions>,
}

/// How often the harness's sockets re-check their session and memberships.
const FAST: Duration = Duration::from_millis(150);

async fn serve(data_service: Arc<PgDataService>, people: &[&Person]) -> Harness {
    serve_revalidating(data_service, people, FAST).await
}

async fn serve_revalidating(
    data_service: Arc<PgDataService>,
    people: &[&Person],
    revalidate: Duration,
) -> Harness {
    serve_with(data_service, people, revalidate, DEFAULT_SEND_DEADLINE).await
}

async fn serve_with(
    data_service: Arc<PgDataService>,
    people: &[&Person],
    revalidate: Duration,
    send_deadline: Duration,
) -> Harness {
    let sessions = Arc::new(Sessions(std::sync::Mutex::new(
        people
            .iter()
            .map(|p| (p.session, support::user_info(p.user_id)))
            .collect(),
    )));
    let broadcast = Arc::new(
        WsBroadcast::new(64)
            .with_revalidate_interval(revalidate)
            .with_send_deadline(send_deadline),
    );
    let mut state = PgAppState::new(
        data_service,
        sessions.clone(),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    state.ws_broadcast = Some(broadcast.clone());
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(ws_handler::<Sessions>))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Harness {
        addr,
        broadcast,
        sessions,
    }
}

/// The public checkout socket needs a full auth service type, though it never
/// authenticates anyone.
async fn serve_checkout(data_service: Arc<PgDataService>) -> (SocketAddr, Arc<WsBroadcast>) {
    let broadcast = Arc::new(WsBroadcast::new(64));
    let mut state = PgAppState::new(
        data_service.clone(),
        Arc::new(auth::WebAuthnAuthService::new(data_service)),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    state.ws_broadcast = Some(broadcast.clone());
    let app = axum::Router::new()
        .route(
            "/checkout/ws",
            axum::routing::get(
                server::api::checkout::checkout_ws_handler::<
                    auth::WebAuthnAuthService<PgDataService>,
                >,
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, broadcast)
}

async fn connect(h: &Harness, person: &Person) -> Socket {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", h.addr))
        .await
        .expect("connect");
    let auth = serde_json::json!({"type": "auth", "token": person.session.0.to_string()});
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();
    let first = next_update(&mut ws).await.expect("connected frame");
    assert_eq!(first["type"], "connected");
    ws
}

async fn next_update(ws: &mut Socket) -> Option<serde_json::Value> {
    let msg = tokio::time::timeout(WAIT, ws.next()).await.ok()??.ok()?;
    serde_json::from_str(msg.to_text().ok()?).ok()
}

/// Asserts that the server closes the socket (Close frame, error or end of
/// stream). A quiet, still-open socket is a failure, not a pass.
async fn assert_closed(ws: &mut Socket, why: &str) {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await {
            Err(_) => panic!("{why}: socket still open after {WAIT:?}"),
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return,
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// Asserts that nothing is delivered within a quiet period.
async fn assert_silent(ws: &mut Socket) {
    if let Ok(Some(Ok(msg))) = tokio::time::timeout(QUIET, ws.next()).await {
        panic!("a frame was delivered that should have been withheld: {msg:?}");
    }
}

fn paid(invoice: &str) -> StatusUpdate {
    StatusUpdate::InvoiceStatus {
        invoice_id: invoice.to_string(),
        status: "paid".to_string(),
    }
}

async fn tenant(pg: &PgDataService, person: &Person, label: &str) -> (Store, UserStore) {
    let store = Store::new(
        format!("ws-{label}-{}", Uuid::new_v4()),
        UserId(person.user_id),
    );
    let membership = pg
        .create_store_owned_by(&store, UserId(person.user_id))
        .await
        .expect("seed store");
    (store, membership)
}

fn sid(store: &Store) -> types::StoreId {
    types::StoreId(store.id.0)
}

async fn make_admin(pool: &PgPool, person: &Person) {
    sqlx::query("UPDATE users SET role = 'server_admin' WHERE id = $1")
        .bind(person.user_id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_tenant_receives_its_own_updates_and_not_another_tenants() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let (a, b) = (Person::new(pg.pool()).await, Person::new(pg.pool()).await);
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let (store_b, _) = tenant(&pg, &b, "b").await;
    let h = serve(pg.clone(), &[&a, &b]).await;
    let mut ws_a = connect(&h, &a).await;

    // B's update is published first, then A's.
    h.broadcast.send(sid(&store_b), paid("inv-of-b"));
    h.broadcast.send(sid(&store_a), paid("inv-of-a"));

    let got = next_update(&mut ws_a).await.expect("A's own update");
    assert_eq!(got["invoice_id"], "inv-of-a", "first frame was {got}");
    assert_silent(&mut ws_a).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn payment_updates_are_scoped_the_same_way() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let (a, b) = (Person::new(pg.pool()).await, Person::new(pg.pool()).await);
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let (store_b, _) = tenant(&pg, &b, "b").await;
    let h = serve(pg.clone(), &[&a, &b]).await;
    let mut ws_a = connect(&h, &a).await;

    let payment = |inv: &str| StatusUpdate::PaymentUpdate {
        payment_id: Uuid::new_v4().to_string(),
        invoice_id: inv.to_string(),
        status: "detected".to_string(),
        amount: Some("1".to_string()),
    };
    h.broadcast.send(sid(&store_b), payment("inv-of-b"));
    h.broadcast.send(sid(&store_a), payment("inv-of-a"));

    let got = next_update(&mut ws_a).await.expect("A's own payment");
    assert_eq!(got["type"], "payment_update");
    assert_eq!(got["invoice_id"], "inv-of-a", "first frame was {got}");
    assert_silent(&mut ws_a).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_server_admin_receives_every_stores_updates() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let (a, b) = (Person::new(pg.pool()).await, Person::new(pg.pool()).await);
    let admin = Person::new(pg.pool()).await;
    make_admin(pg.pool(), &admin).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let (store_b, _) = tenant(&pg, &b, "b").await;
    let h = serve(pg.clone(), &[&admin]).await;
    let mut ws = connect(&h, &admin).await;

    h.broadcast.send(sid(&store_b), paid("inv-of-b"));
    h.broadcast.send(sid(&store_a), paid("inv-of-a"));

    assert_eq!(
        next_update(&mut ws).await.unwrap()["invoice_id"],
        "inv-of-b"
    );
    assert_eq!(
        next_update(&mut ws).await.unwrap()["invoice_id"],
        "inv-of-a"
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_member_removed_mid_connection_stops_receiving() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let owner = Person::new(pg.pool()).await;
    let member = Person::new(pg.pool()).await;
    let admin = Person::new(pg.pool()).await;
    make_admin(pg.pool(), &admin).await;
    let (store, owner_membership) = tenant(&pg, &owner, "shared").await;
    pg.add_user_to_store(&UserStore::new(
        UserId(member.user_id),
        store.id,
        owner_membership.store_role_id,
    ))
    .await
    .unwrap();
    let h = serve(pg.clone(), &[&member, &admin]).await;
    let mut ws_member = connect(&h, &member).await;
    let mut ws_admin = connect(&h, &admin).await;

    // While still a member: delivered. This is the positive control.
    h.broadcast.send(sid(&store), paid("before-removal"));
    assert_eq!(
        next_update(&mut ws_member).await.expect("member sees it")["invoice_id"],
        "before-removal"
    );
    assert_eq!(
        next_update(&mut ws_admin).await.unwrap()["invoice_id"],
        "before-removal"
    );

    pg.remove_user_from_store(UserId(member.user_id), store.id)
        .await
        .unwrap();

    // The socket is still open. Once it has re-checked its memberships, the
    // next event must not reach it.
    tokio::time::sleep(FAST * 4).await;
    h.broadcast.send(sid(&store), paid("after-removal"));
    assert_eq!(
        next_update(&mut ws_admin).await.unwrap()["invoice_id"],
        "after-removal",
        "the event was published and reached an entitled socket"
    );
    assert_silent(&mut ws_member).await;
}

/// The access decision is taken on the event, not read from a map a timer
/// keeps up to date. The periodic re-check is set to an hour, so no tick can
/// run during the test: if the removal is not acted on at the moment the next
/// event is forwarded, the ex-member receives it.
#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_member_removed_mid_connection_stops_receiving_on_the_next_event() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let owner = Person::new(pg.pool()).await;
    let member = Person::new(pg.pool()).await;
    let admin = Person::new(pg.pool()).await;
    make_admin(pg.pool(), &admin).await;
    let (store, owner_membership) = tenant(&pg, &owner, "shared").await;
    pg.add_user_to_store(&UserStore::new(
        UserId(member.user_id),
        store.id,
        owner_membership.store_role_id,
    ))
    .await
    .unwrap();
    let h = serve_revalidating(pg.clone(), &[&member, &admin], Duration::from_secs(3600)).await;
    let mut ws_member = connect(&h, &member).await;
    let mut ws_admin = connect(&h, &admin).await;

    // While still a member: delivered. This is the positive control.
    h.broadcast.send(sid(&store), paid("before-removal"));
    assert_eq!(
        next_update(&mut ws_member).await.expect("member sees it")["invoice_id"],
        "before-removal"
    );
    assert_eq!(
        next_update(&mut ws_admin).await.unwrap()["invoice_id"],
        "before-removal"
    );

    pg.remove_user_from_store(UserId(member.user_id), store.id)
        .await
        .unwrap();

    // The very next event, with nothing waited for in between.
    h.broadcast.send(sid(&store), paid("after-removal"));
    assert_eq!(
        next_update(&mut ws_admin).await.unwrap()["invoice_id"],
        "after-removal",
        "the event was published and reached an entitled socket"
    );
    assert_silent(&mut ws_member).await;
}

/// A logout is acted on at the next event too, with no tick available.
#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_revoked_session_closes_the_socket_on_the_next_event() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let a = Person::new(pg.pool()).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let h = serve_revalidating(pg.clone(), &[&a], Duration::from_secs(3600)).await;
    let mut ws_a = connect(&h, &a).await;

    h.broadcast.send(sid(&store_a), paid("while-logged-in"));
    assert_eq!(
        next_update(&mut ws_a).await.expect("delivered")["invoice_id"],
        "while-logged-in"
    );

    // Logout: the user still belongs to the store, and no re-check is due.
    h.sessions.revoke(a.session);
    h.broadcast.send(sid(&store_a), paid("after-logout"));

    assert_closed(&mut ws_a, "session revoked, next event").await;
}

/// How many frames the stalled-client test publishes, and how large each is.
///
/// Their product has to exceed what the kernel will buffer on both ends of a
/// loopback socket, or the server's write never blocks and the test fails for
/// the wrong reason. Linux's defaults cap that at roughly 4MB of send buffer
/// plus 6MB of receive buffer, so 24MB clears it several times over. The count
/// stays below the broadcast capacity the harness uses, so the burst cannot
/// lag a listener and the only thing that can close the socket is the write
/// deadline. The payload rides in a field the wire format already carries.
const STALL_FRAMES: usize = 48;
const STALL_FRAME_BYTES: usize = 512 * 1024;

fn bulky(invoice: &str) -> StatusUpdate {
    StatusUpdate::InvoiceStatus {
        invoice_id: invoice.to_string(),
        status: "x".repeat(STALL_FRAME_BYTES),
    }
}

/// Backpressure must end access, not extend it. Everything else that could
/// close this socket is held off: the session stays valid, the membership
/// stays, no re-check is due for an hour, and the burst is well inside the
/// channel capacities so nothing lags. The only thing left that can close it
/// is the bounded write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs DATABASE_URL"]
async fn a_client_that_stops_reading_loses_its_socket() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let a = Person::new(pg.pool()).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let deadline = Duration::from_millis(500);
    let h = serve_with(pg.clone(), &[&a], Duration::from_secs(3600), deadline).await;
    let mut ws_a = connect(&h, &a).await;

    // From here the client reads nothing. Every update is one it is entitled
    // to, so refusing to read is the only thing wrong with it.
    for i in 0..STALL_FRAMES {
        h.broadcast
            .send(sid(&store_a), bulky(&format!("stall-{i}")));
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(deadline * 4).await;

    // Reading now drains what was buffered and then reaches the close the
    // server already decided on.
    assert_closed(&mut ws_a, "the client stopped reading").await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn when_the_decision_cannot_be_made_the_socket_is_closed() {
    let pg = support::service().await;
    let url = data_service::test_support::database_url();
    // The handler gets its own pool so that closing it breaks only the
    // handler's reads, not the seeding done through the shared one.
    let handler_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let a = Person::new(pg.pool()).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let h = serve(Arc::new(PgDataService::new(handler_pool.clone())), &[&a]).await;
    let mut ws_a = connect(&h, &a).await;

    h.broadcast.send(sid(&store_a), paid("healthy"));
    assert_eq!(
        next_update(&mut ws_a)
            .await
            .expect("delivered while healthy")["invoice_id"],
        "healthy"
    );

    handler_pool.close().await;

    // The entitlement can no longer be established, so the socket closes
    // instead of carrying on with the last answer it had.
    assert_closed(&mut ws_a, "entitlement undecidable").await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_revoked_session_closes_the_socket_though_membership_remains() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let a = Person::new(pg.pool()).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let h = serve(pg.clone(), &[&a]).await;
    let mut ws_a = connect(&h, &a).await;

    h.broadcast.send(sid(&store_a), paid("while-logged-in"));
    assert_eq!(
        next_update(&mut ws_a).await.expect("delivered")["invoice_id"],
        "while-logged-in"
    );

    // Logout: the user still belongs to the store.
    h.sessions.revoke(a.session);

    assert_closed(&mut ws_a, "session revoked").await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_store_created_mid_connection_is_heard_only_after_the_next_recheck() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let a = Person::new(pg.pool()).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;

    // Until the socket re-checks, a store joined after connecting has no
    // channel on it: an update published then is not delivered, and is not
    // replayed later. This pins the late-grant window as it is.
    let slow = serve_revalidating(pg.clone(), &[&a], Duration::from_secs(3600)).await;
    let mut ws_slow = connect(&slow, &a).await;
    let (late_store, _) = tenant(&pg, &a, "late").await;
    slow.broadcast
        .send(sid(&late_store), paid("before-recheck"));
    slow.broadcast.send(sid(&store_a), paid("positive-control"));
    let got = next_update(&mut ws_slow).await.expect("the old store");
    assert_eq!(
        got["invoice_id"], "positive-control",
        "first frame was {got}"
    );
    assert_silent(&mut ws_slow).await;

    // After a re-check the new store is heard.
    let fast = serve(pg.clone(), &[&a]).await;
    let mut ws_fast = connect(&fast, &a).await;
    let (later_store, _) = tenant(&pg, &a, "later").await;
    tokio::time::sleep(FAST * 4).await;
    fast.broadcast
        .send(sid(&later_store), paid("after-recheck"));
    let got = next_update(&mut ws_fast).await.expect("the new store");
    assert_eq!(got["invoice_id"], "after-recheck", "first frame was {got}");
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn routing_alone_keeps_another_tenants_update_away() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let (a, b) = (Person::new(pg.pool()).await, Person::new(pg.pool()).await);
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let (store_b, _) = tenant(&pg, &b, "b").await;
    // The socket never re-derives its subscriptions while the test runs, so
    // only the channels it took at connect could deliver anything - and B's
    // store is not among them, so B's update has nowhere to arrive.
    let h = serve_revalidating(pg.clone(), &[&a], Duration::from_secs(3600)).await;
    let mut ws_a = connect(&h, &a).await;

    h.broadcast.send(sid(&store_b), paid("inv-of-b"));
    h.broadcast.send(sid(&store_a), paid("inv-of-a"));

    let got = next_update(&mut ws_a).await.expect("A's own update");
    assert_eq!(got["invoice_id"], "inv-of-a", "first frame was {got}");
    assert_silent(&mut ws_a).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_checkout_socket_receives_only_its_own_invoice() {
    let pg = support::service().await;
    let (a, b) = (
        support::seed_tenant(&pg, "a").await,
        support::seed_tenant(&pg, "b").await,
    );
    let (addr, broadcast) = serve_checkout(Arc::new(pg)).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/checkout/ws?invoice_id={}",
        addr,
        a.invoice.id.as_str()
    ))
    .await
    .expect("connect");
    assert_eq!(next_update(&mut ws).await.unwrap()["type"], "connected");

    broadcast.send(types::StoreId(b.store.id.0), paid(b.invoice.id.as_str()));
    broadcast.send(types::StoreId(a.store.id.0), paid(a.invoice.id.as_str()));

    let got = next_update(&mut ws).await.expect("own invoice's update");
    assert_eq!(
        got["invoice_id"],
        a.invoice.id.as_str(),
        "first frame was {got}"
    );
    assert_silent(&mut ws).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn an_update_for_an_invoice_is_attributed_to_the_invoices_store() {
    let pg = support::service().await;
    let (a, b) = (
        support::seed_tenant(&pg, "a").await,
        support::seed_tenant(&pg, "b").await,
    );
    let broadcast = WsBroadcast::new(16);
    let mut observe_a = broadcast.subscribe_store(types::StoreId(a.store.id.0));
    let mut observe_b = broadcast.subscribe_store(types::StoreId(b.store.id.0));

    // An invoice that does not exist has no store, so nothing is published.
    broadcast
        .send_for_invoice(&pg, &types::InvoiceId::new(), paid("ghost"))
        .await;
    broadcast
        .send_for_invoice(&pg, &b.invoice.id, paid("b"))
        .await;
    broadcast
        .send_for_invoice(&pg, &a.invoice.id, paid("a"))
        .await;

    assert!(matches!(
        observe_b.recv().await.unwrap(),
        StatusUpdate::InvoiceStatus { invoice_id, .. } if invoice_id == "b"
    ));
    assert!(matches!(
        observe_a.recv().await.unwrap(),
        StatusUpdate::InvoiceStatus { invoice_id, .. } if invoice_id == "a"
    ));
    assert!(observe_a.try_recv().is_err(), "nothing else was published");
    assert!(observe_b.try_recv().is_err(), "nothing else was published");
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_socket_that_missed_updates_is_closed_so_the_client_resyncs() {
    let pg = support::service().await;
    let pg = Arc::new(pg);
    let a = Person::new(pg.pool()).await;
    let (store_a, _) = tenant(&pg, &a, "a").await;
    let h = serve_revalidating(pg.clone(), &[&a], Duration::from_secs(3600)).await;
    let mut ws_a = connect(&h, &a).await;

    // The test runtime is single-threaded and this loop never yields, so the
    // socket's listener cannot drain the 64-slot channel while it fills.
    for i in 0..200 {
        h.broadcast.send(sid(&store_a), paid(&format!("burst-{i}")));
    }

    assert_closed(&mut ws_a, "updates were dropped").await;
}
