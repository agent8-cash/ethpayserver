//! WebSocket endpoint for real-time invoice and payment status updates.
//!
//! Clients connect to `/ws` and authenticate by sending an auth message as the
//! first frame: `{"type":"auth","token":"SESSION_ID"}`. The server validates
//! the session and then forwards JSON-encoded status updates.
//!
//! Every update is tied to the store it happened in and is published only to
//! that store's channel (and its invoice's, and the server admins'). A socket
//! subscribes to the channels of the stores its user may see - all of them for
//! a server admin - so another store's update never reaches it, rather than
//! reaching it and being filtered.
//!
//! Routing decides what can arrive; it does not decide what may be sent. The
//! authorization decision is made per event, against the database, immediately
//! before each send - not read from the subscription map, which a timer keeps
//! up to date. A membership removed, a role lost or a session ended since the
//! update was queued stops it on that event, whatever any loop is doing. A
//! logged-out or revoked session, or a user that no longer exists, closes the
//! socket; anything that prevents the decision (a database error) closes it
//! too: it fails closed.
//!
//! A client that stops reading must not be able to hold its access open by
//! declining to read. Each send is bounded by [`DEFAULT_SEND_DEADLINE`] and a
//! client that exceeds it loses its socket, so backpressure ends access rather
//! than extending it - and the per-event check cannot be stalled by a peer.
//!
//! The periodic re-derivation is subscription upkeep, not the control: it adds
//! channels for stores newly joined and drops those no longer allowed.

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use auth::{SessionService, UserRepository, repository::UserStoreRepository};
use types::{InvoiceId, InvoiceReader, StoreId};

use super::invoices::{StoreScope, verify_store_access_for_query};
use crate::state::PgAppState;

/// Client-to-server messages.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    /// Authentication message — must be the first frame after connection.
    #[serde(rename = "auth")]
    Auth { token: String },
}

/// Status update sent to WebSocket clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StatusUpdate {
    /// Invoice status changed.
    #[serde(rename = "invoice_status")]
    InvoiceStatus { invoice_id: String, status: String },
    /// Payment received or updated.
    #[serde(rename = "payment_update")]
    PaymentUpdate {
        payment_id: String,
        invoice_id: String,
        status: String,
        amount: Option<String>,
    },
    /// Connection acknowledged.
    #[serde(rename = "connected")]
    Connected,
    /// Server-sent ping.
    #[serde(rename = "ping")]
    Ping,
}

/// Which channel a socket is listening to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Topic {
    /// Every store on the server. Server admins only.
    Admin,
    /// One store.
    Store(StoreId),
}

/// What one authenticated socket is entitled to listen to right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Entitlement {
    topics: std::collections::HashSet<Topic>,
}

/// Broadcast channels keyed by what they carry. A channel exists only while
/// someone listens to it.
struct Channels<K> {
    capacity: usize,
    map: std::sync::Mutex<std::collections::HashMap<K, broadcast::Sender<StatusUpdate>>>,
}

impl<K: std::hash::Hash + Eq + Clone> Channels<K> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: Default::default(),
        }
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<K, broadcast::Sender<StatusUpdate>>>
    {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn subscribe(&self, key: &K) -> broadcast::Receiver<StatusUpdate> {
        let mut map = self.lock();
        // Drop channels nobody listens to any more, so keys that were
        // subscribed once and never published to do not accumulate.
        map.retain(|_, tx| tx.receiver_count() > 0);
        map.entry(key.clone())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .subscribe()
    }

    fn publish(&self, key: &K, update: &StatusUpdate) {
        let mut map = self.lock();
        if let Some(tx) = map.get(key)
            && tx.send(update.clone()).is_err()
        {
            // `send` only fails when there are no receivers.
            map.remove(key);
        }
    }
}

/// Status updates, routed so that a listener only ever holds a channel that
/// carries what it may see.
///
/// There is one channel per store, one per invoice (for the public checkout
/// socket, where the invoice id is the capability), and one for server admins.
/// An update is published to the channels of its own store and invoice and to
/// nothing else, so a socket that never subscribed to a store has no way to
/// receive its events: the data does not reach it, rather than reaching it
/// and being discarded.
#[derive(Clone)]
pub struct WsBroadcast {
    stores: std::sync::Arc<Channels<StoreId>>,
    invoices: std::sync::Arc<Channels<String>>,
    admins: std::sync::Arc<Channels<()>>,
    revalidate_interval: std::time::Duration,
    send_deadline: std::time::Duration,
}

/// How often an open `/ws` socket re-derives which channels it subscribes to.
///
/// This is upkeep, not the access decision: that is made per event, just
/// before each send. It bounds only how long a store joined mid-connection
/// goes unsubscribed, and how long an unentitled channel stays subscribed
/// while delivering nothing.
pub const DEFAULT_REVALIDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// How long one frame may take to reach a client before its socket is closed.
///
/// A client that stops reading applies backpressure through the kernel buffers
/// back to the server's send. Waiting on that without a bound would let a peer
/// suspend the forwarding loop, and with it the per-event authorization check,
/// for as long as it declines to read; a revoked member could then hold its
/// subscription open simply by not reading. The wait is therefore bounded and
/// a client that exceeds it loses its socket: backpressure terminates access
/// instead of extending it. A live client acknowledges a frame of a few
/// hundred bytes far inside this; one that does not is gone, and reconnects
/// and refetches.
pub const DEFAULT_SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

impl WsBroadcast {
    /// Create broadcast channels with the given per-channel capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            stores: std::sync::Arc::new(Channels::new(capacity)),
            invoices: std::sync::Arc::new(Channels::new(capacity)),
            admins: std::sync::Arc::new(Channels::new(capacity)),
            revalidate_interval: DEFAULT_REVALIDATE_INTERVAL,
            send_deadline: DEFAULT_SEND_DEADLINE,
        }
    }

    /// Set how often an open socket re-derives its subscriptions. This bounds
    /// how long a store joined mid-connection goes unheard; it does not bound
    /// revocation, which takes effect on the next event.
    pub fn with_revalidate_interval(mut self, interval: std::time::Duration) -> Self {
        self.revalidate_interval = interval;
        self
    }

    /// Set how long a single send may block on a client before its socket is
    /// closed. See [`DEFAULT_SEND_DEADLINE`].
    pub fn with_send_deadline(mut self, deadline: std::time::Duration) -> Self {
        self.send_deadline = deadline;
        self
    }

    /// Publish a status update for `store_id`.
    ///
    /// The store is required, not optional: an update with no store would have
    /// no channel it could safely go to.
    pub fn send(&self, store_id: StoreId, update: StatusUpdate) {
        let invoice_id = match &update {
            StatusUpdate::InvoiceStatus { invoice_id, .. }
            | StatusUpdate::PaymentUpdate { invoice_id, .. } => Some(invoice_id.clone()),
            StatusUpdate::Connected | StatusUpdate::Ping => None,
        };
        self.stores.publish(&store_id, &update);
        self.admins.publish(&(), &update);
        if let Some(invoice_id) = invoice_id {
            self.invoices.publish(&invoice_id, &update);
        }
    }

    /// Publish a status update for an invoice whose store the caller does not
    /// already hold, looking the store up from the invoice.
    ///
    /// If the store cannot be determined - the lookup fails, or the invoice is
    /// gone - nothing is published: an update that cannot be attributed to a
    /// store cannot be shown to anyone.
    pub async fn send_for_invoice<R>(
        &self,
        reader: &R,
        invoice_id: &InvoiceId,
        update: StatusUpdate,
    ) where
        R: InvoiceReader + ?Sized,
    {
        match reader.get(invoice_id).await {
            Ok(Some(invoice)) => self.send(invoice.store_id, update),
            Ok(None) => tracing::warn!(
                invoice_id = %invoice_id.as_str(),
                "status update dropped: invoice not found, so its store is unknown"
            ),
            Err(e) => tracing::warn!(
                invoice_id = %invoice_id.as_str(),
                error = %e,
                "status update dropped: invoice lookup failed, so its store is unknown"
            ),
        }
    }

    /// Subscribe to the updates of one store.
    pub fn subscribe_store(&self, store_id: StoreId) -> broadcast::Receiver<StatusUpdate> {
        self.stores.subscribe(&store_id)
    }

    /// Subscribe to the updates of one invoice.
    pub fn subscribe_invoice(&self, invoice_id: &str) -> broadcast::Receiver<StatusUpdate> {
        self.invoices.subscribe(&invoice_id.to_string())
    }

    fn subscribe_topic(&self, topic: Topic) -> broadcast::Receiver<StatusUpdate> {
        match topic {
            Topic::Admin => self.admins.subscribe(&()),
            Topic::Store(id) => self.subscribe_store(id),
        }
    }
}

/// Re-derives, for one socket, whether its session is still valid and which
/// topics its user may listen to. `None` means the socket must be closed.
type Revalidate = std::sync::Arc<dyn Fn() -> BoxFuture<'static, Option<Entitlement>> + Send + Sync>;

/// The topics `user_id` may listen to: everything for a server admin, else
/// the stores they are a member of.
///
/// The user is read afresh, so a demotion takes effect at the next
/// re-validation, and the scope is the one the REST list endpoints use. Fails
/// closed: every error, and a user row that is gone, answers `None`.
pub(crate) async fn entitlement_of<D>(
    data_service: &D,
    user_id: auth::UserId,
) -> Option<Entitlement>
where
    D: UserRepository + UserStoreRepository + ?Sized,
{
    let user = match data_service.get_user(user_id).await {
        Ok(Some(user)) => auth::UserInfo::from(&user),
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "ws: could not read the user; closing the socket");
            return None;
        }
    };
    let topics = match verify_store_access_for_query(data_service, &user, None).await {
        Ok(StoreScope::All) => [Topic::Admin].into_iter().collect(),
        Ok(StoreScope::Membership(stores)) => stores.into_iter().map(Topic::Store).collect(),
        Ok(StoreScope::One(_)) => return None,
        Err(status) => {
            tracing::warn!(%status, "ws: could not read memberships; closing the socket");
            return None;
        }
    };
    Some(Entitlement { topics })
}

/// WebSocket upgrade handler.
///
/// Accepts the upgrade immediately. Authentication happens inside the
/// WebSocket session: the client must send `{"type":"auth","token":"..."}` as
/// the first frame. This avoids leaking the session token in URL query
/// parameters (browser history, proxy logs, server access logs).
pub async fn ws_handler<A>(
    ws: WebSocketUpgrade,
    State(state): State<PgAppState<A>>,
) -> impl IntoResponse
where
    A: SessionService + 'static,
{
    // In deployments without WS configured, return 503 rather than panicking
    // the handler task.
    let Some(ws_broadcast) = state.ws_broadcast.as_ref() else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let broadcast = ws_broadcast.as_ref().clone();
    let auth_service = state.auth_service.clone();
    let data_service = state.data_service.clone();

    ws.on_upgrade(move |socket| handle_socket(socket, broadcast, auth_service, data_service))
        .into_response()
}

/// Maximum time to wait for the auth message before closing the connection.
const AUTH_TIMEOUT_SECS: u64 = 10;

/// Handle an individual WebSocket connection.
///
/// Waits for the client to send an auth message, validates the session,
/// then forwards the updates of the channels the user is entitled to.
async fn handle_socket<A: SessionService + 'static>(
    socket: WebSocket,
    broadcast: WsBroadcast,
    auth_service: std::sync::Arc<A>,
    data_service: std::sync::Arc<data_service::PgDataService>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Wait for auth message from client
    let auth_result = tokio::time::timeout(
        std::time::Duration::from_secs(AUTH_TIMEOUT_SECS),
        receiver.next(),
    )
    .await;

    let token = match auth_result {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::Auth { token }) => token,
            Err(_) => {
                let _ = sender.close().await;
                return;
            }
        },
        _ => {
            let _ = sender.close().await;
            return;
        }
    };

    // Validate session token
    let session_id = match uuid::Uuid::parse_str(&token) {
        Ok(uuid) => auth::SessionId(uuid),
        Err(_) => {
            let _ = sender.close().await;
            return;
        }
    };

    // The session is checked again on every tick, not just here: a logged-out
    // or revoked session must not keep an open socket.
    let revalidate: Revalidate = std::sync::Arc::new(move || {
        let auth_service = auth_service.clone();
        let data_service = data_service.clone();
        Box::pin(async move {
            // An error here may be the backend failing rather than a revoked
            // session; either way the socket closes, but say which.
            let (user, _session) = match auth_service.validate_session(session_id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::info!(error = %e, "ws: session no longer valid or unreadable; closing the socket");
                    return None;
                }
            };
            entitlement_of(&*data_service, user.id).await
        })
    });

    handle_socket_forwarding(sender, receiver, broadcast, revalidate).await;
}

/// Aborts its task when dropped, so a socket's listeners die with it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Make `listeners` match `entitlement`: start a listener for each topic newly
/// allowed, and drop (and so abort) the listener of each topic no longer
/// allowed.
fn sync_listeners(
    broadcast: &WsBroadcast,
    entitlement: &Entitlement,
    listeners: &mut std::collections::HashMap<Topic, AbortOnDrop>,
    out: &tokio::sync::mpsc::Sender<(Topic, StatusUpdate)>,
    lagged: &tokio::sync::mpsc::Sender<()>,
) {
    listeners.retain(|topic, _| entitlement.topics.contains(topic));
    for topic in &entitlement.topics {
        if listeners.contains_key(topic) {
            continue;
        }
        let mut rx = broadcast.subscribe_topic(*topic);
        let out = out.clone();
        let lagged = lagged.clone();
        let topic = *topic;
        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(update) => {
                        if out.send((topic, update)).await.is_err() {
                            break;
                        }
                    }
                    // Dropped updates may include a payment confirmation and
                    // the client cannot know it missed one, so the socket is
                    // closed and the client resyncs on reconnect.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = lagged.try_send(());
                        break;
                    }
                    // A channel that closed under a live listener would leave
                    // the socket connected but deaf; fail closed instead.
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = lagged.try_send(());
                        break;
                    }
                }
            }
        });
        listeners.insert(topic, AbortOnDrop(task));
    }
}

/// Send one text frame, waiting at most `deadline` for the client to take it.
///
/// `false` means the socket must be closed: the write failed, or the client
/// did not accept the frame in time. The deadline is what stops a peer that
/// has stopped reading from parking the caller - and so the per-event
/// authorization check - for as long as it likes. See
/// [`DEFAULT_SEND_DEADLINE`].
async fn send_within(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    msg: String,
    deadline: std::time::Duration,
) -> bool {
    match tokio::time::timeout(deadline, sender.send(Message::Text(msg.into()))).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            tracing::info!(
                "ws: the client did not accept a frame within the send deadline; closing the socket"
            );
            false
        }
    }
}

/// Forward updates to an authenticated WebSocket client, until it closes or
/// its session or entitlement can no longer be established.
async fn handle_socket_forwarding(
    mut sender: futures::stream::SplitSink<WebSocket, Message>,
    mut receiver: futures::stream::SplitStream<WebSocket>,
    broadcast: WsBroadcast,
    revalidate: Revalidate,
) {
    let Some(entitlement) = revalidate().await else {
        let _ = sender.close().await;
        return;
    };
    let (out, mut updates) = tokio::sync::mpsc::channel::<(Topic, StatusUpdate)>(256);
    let (lagged, mut lagged_rx) = tokio::sync::mpsc::channel::<()>(1);
    let mut listeners = std::collections::HashMap::new();
    // Subscribe before acknowledging, so nothing published after the client
    // sees `connected` can be missed.
    sync_listeners(&broadcast, &entitlement, &mut listeners, &out, &lagged);

    // Send connected acknowledgement. Serialising a unit-variant is infallible.
    #[allow(
        clippy::unwrap_used,
        reason = "serde_json of unit variant is infallible"
    )]
    let connected = serde_json::to_string(&StatusUpdate::Connected).unwrap();
    let send_deadline = broadcast.send_deadline;
    if !send_within(&mut sender, connected, send_deadline).await {
        return;
    }

    let interval = broadcast.revalidate_interval;
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some((topic, update)) = updates.recv() => {
                // The access decision is made here, on this event, from the
                // database - not read back from `listeners`, which a timer
                // refreshes. An entitlement withdrawn since this update was
                // queued, or since the last tick, stops it now.
                let Some(entitlement) = revalidate().await else { break };
                sync_listeners(&broadcast, &entitlement, &mut listeners, &out, &lagged);
                if !entitlement.topics.contains(&topic) {
                    continue;
                }
                let Ok(msg) = serde_json::to_string(&update) else { continue };
                // A client that will not take the frame loses the socket, so
                // declining to read cannot suspend the check above.
                if !send_within(&mut sender, msg, send_deadline).await {
                    break;
                }
            }
            Some(()) = lagged_rx.recv() => break,
            // Upkeep, on a socket that happens to be idle: pick up a store
            // joined since the last pass, and stop holding a channel whose
            // entitlement is gone. Delivery is decided above, per event.
            _ = ticker.tick() => match revalidate().await {
                Some(entitlement) => sync_listeners(&broadcast, &entitlement, &mut listeners, &out, &lagged),
                None => break,
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
    // The close handshake is a write too, so it gets the same deadline: a peer
    // that reads nothing must not keep this task alive either.
    let _ = tokio::time::timeout(send_deadline, sender.close()).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::routing;
    use futures::StreamExt;

    fn store() -> StoreId {
        StoreId(uuid::Uuid::from_bytes([7; 16]))
    }

    #[test]
    fn test_status_update_serde_invoice_status() {
        let update = StatusUpdate::InvoiceStatus {
            invoice_id: "inv_1".to_string(),
            status: "paid".to_string(),
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["type"], "invoice_status");
        assert_eq!(json["invoice_id"], "inv_1");
        assert_eq!(json["status"], "paid");

        let parsed: StatusUpdate = serde_json::from_value(json).unwrap();
        assert!(
            matches!(parsed, StatusUpdate::InvoiceStatus { invoice_id, status } if invoice_id == "inv_1" && status == "paid")
        );
    }

    #[test]
    fn test_status_update_serde_payment_update() {
        let update = StatusUpdate::PaymentUpdate {
            payment_id: "pay_1".to_string(),
            invoice_id: "inv_1".to_string(),
            status: "confirmed".to_string(),
            amount: Some("1.5".to_string()),
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["type"], "payment_update");
        assert_eq!(json["payment_id"], "pay_1");
        assert_eq!(json["amount"], "1.5");

        // amount = None
        let update_no_amount = StatusUpdate::PaymentUpdate {
            payment_id: "pay_2".to_string(),
            invoice_id: "inv_2".to_string(),
            status: "detecting".to_string(),
            amount: None,
        };
        let json2 = serde_json::to_value(&update_no_amount).unwrap();
        assert!(json2.get("amount").unwrap().is_null());
    }

    #[test]
    fn test_status_update_serde_connected_and_ping() {
        let connected_json = serde_json::to_string(&StatusUpdate::Connected).unwrap();
        assert_eq!(connected_json, r#"{"type":"connected"}"#);

        let ping_json = serde_json::to_string(&StatusUpdate::Ping).unwrap();
        assert_eq!(ping_json, r#"{"type":"ping"}"#);

        let parsed: StatusUpdate = serde_json::from_str(&connected_json).unwrap();
        assert!(matches!(parsed, StatusUpdate::Connected));
    }

    #[test]
    fn test_client_message_auth_deserialize() {
        let json = r#"{"type":"auth","token":"abc-123"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Auth { token } if token == "abc-123"));
    }

    #[test]
    fn test_client_message_missing_type_fails() {
        let json = r#"{"token":"abc-123"}"#;
        assert!(serde_json::from_str::<ClientMessage>(json).is_err());
    }

    #[test]
    fn test_ws_broadcast_send_no_receivers() {
        let broadcast = WsBroadcast::new(16);
        // Should not panic even with no receivers
        broadcast.send(store(), StatusUpdate::Ping);
    }

    fn paid(invoice_id: &str) -> StatusUpdate {
        StatusUpdate::InvoiceStatus {
            invoice_id: invoice_id.to_string(),
            status: "paid".to_string(),
        }
    }

    #[tokio::test]
    async fn test_ws_broadcast_send_receive() {
        let broadcast = WsBroadcast::new(16);
        let mut rx = broadcast.subscribe_store(store());

        broadcast.send(store(), StatusUpdate::Connected);
        broadcast.send(store(), paid("inv_1"));

        assert!(matches!(rx.recv().await.unwrap(), StatusUpdate::Connected));
        assert!(
            matches!(rx.recv().await.unwrap(), StatusUpdate::InvoiceStatus { invoice_id, .. } if invoice_id == "inv_1")
        );
    }

    #[tokio::test]
    async fn test_ws_broadcast_multiple_subscribers() {
        let broadcast = WsBroadcast::new(16);
        let mut rx1 = broadcast.subscribe_store(store());
        let mut rx2 = broadcast.subscribe_store(store());

        broadcast.send(store(), StatusUpdate::Ping);

        assert!(matches!(rx1.recv().await.unwrap(), StatusUpdate::Ping));
        assert!(matches!(rx2.recv().await.unwrap(), StatusUpdate::Ping));
    }

    #[tokio::test]
    async fn a_store_channel_never_carries_another_stores_update() {
        let broadcast = WsBroadcast::new(16);
        let other = StoreId(uuid::Uuid::from_bytes([8; 16]));
        let mut mine = broadcast.subscribe_store(store());
        let mut theirs = broadcast.subscribe_store(other);

        broadcast.send(other, paid("inv_other"));
        broadcast.send(store(), paid("inv_mine"));

        assert!(
            matches!(mine.try_recv().unwrap(), StatusUpdate::InvoiceStatus { invoice_id, .. } if invoice_id == "inv_mine")
        );
        assert!(mine.try_recv().is_err(), "nothing else was routed here");
        assert!(
            matches!(theirs.try_recv().unwrap(), StatusUpdate::InvoiceStatus { invoice_id, .. } if invoice_id == "inv_other")
        );
        assert!(theirs.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_invoice_channel_carries_only_that_invoice() {
        let broadcast = WsBroadcast::new(16);
        let mut rx = broadcast.subscribe_invoice("inv_a");

        // Same store, different invoice: still not for this channel.
        broadcast.send(store(), paid("inv_b"));
        broadcast.send(store(), paid("inv_a"));

        assert!(
            matches!(rx.try_recv().unwrap(), StatusUpdate::InvoiceStatus { invoice_id, .. } if invoice_id == "inv_a")
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_admin_channel_carries_every_store() {
        let broadcast = WsBroadcast::new(16);
        let mut rx = broadcast.subscribe_topic(Topic::Admin);
        broadcast.send(store(), paid("inv_1"));
        broadcast.send(StoreId(uuid::Uuid::from_bytes([8; 16])), paid("inv_2"));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_ws_broadcast_capacity_overflow_lags_receiver() {
        let broadcast = WsBroadcast::new(2);
        let mut rx = broadcast.subscribe_store(store());

        // Send more messages than the channel capacity
        broadcast.send(store(), StatusUpdate::Ping);
        broadcast.send(store(), StatusUpdate::Connected);
        broadcast.send(store(), StatusUpdate::Ping);

        // The receiver should report lagged (missed messages)
        let result = rx.recv().await;
        assert!(
            matches!(result, Err(broadcast::error::RecvError::Lagged(count)) if count > 0),
            "expected Lagged error, got {result:?}"
        );

        // After the lag error, the remaining buffered messages are still receivable
        assert!(matches!(rx.recv().await.unwrap(), StatusUpdate::Connected));
        assert!(matches!(rx.recv().await.unwrap(), StatusUpdate::Ping));
    }

    /// Helper handler for transport tests — upgrades to WebSocket and delegates
    /// to `handle_socket_forwarding` with an entitlement to the one test store.
    /// Which stores a socket may see is covered through the real `/ws` handler
    /// in `server/tests/ws_store_scope.rs`.
    async fn test_upgrade(
        ws: WebSocketUpgrade,
        axum::extract::State(bc): axum::extract::State<WsBroadcast>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| {
            let (sender, receiver) = socket.split();
            let entitled: Revalidate = std::sync::Arc::new(|| {
                Box::pin(async {
                    Some(Entitlement {
                        topics: [Topic::Store(store())].into_iter().collect(),
                    })
                })
            });
            handle_socket_forwarding(sender, receiver, bc, entitled)
        })
    }

    /// Spin up a one-shot axum server that exposes `handle_socket` at `/ws`.
    /// Returns the server address and the `WsBroadcast` sender.
    async fn spawn_test_ws_server() -> (std::net::SocketAddr, WsBroadcast) {
        let broadcast = WsBroadcast::new(16);
        let app = axum::Router::new()
            .route("/ws", routing::get(test_upgrade))
            .with_state(broadcast.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (addr, broadcast)
    }

    #[tokio::test]
    async fn test_handle_socket_sends_connected_on_open() {
        let (addr, _broadcast) = spawn_test_ws_server().await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("client connect failed");

        // First message must be the Connected acknowledgement
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for Connected")
            .unwrap()
            .unwrap();
        let update: StatusUpdate = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert!(matches!(update, StatusUpdate::Connected));
    }

    #[tokio::test]
    async fn test_handle_socket_forwards_broadcast() {
        let timeout = std::time::Duration::from_secs(5);
        let (addr, broadcast) = spawn_test_ws_server().await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("client connect failed");

        // Consume the Connected message
        tokio::time::timeout(timeout, ws.next())
            .await
            .expect("timed out waiting for Connected")
            .unwrap()
            .unwrap();

        // Broadcast a status update
        broadcast.send(
            store(),
            StatusUpdate::InvoiceStatus {
                invoice_id: "inv_42".to_string(),
                status: "paid".to_string(),
            },
        );

        let msg = tokio::time::timeout(timeout, ws.next())
            .await
            .expect("timed out waiting for broadcast")
            .unwrap()
            .unwrap();
        let update: StatusUpdate = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert!(
            matches!(update, StatusUpdate::InvoiceStatus { ref invoice_id, ref status }
                if invoice_id == "inv_42" && status == "paid")
        );
    }

    #[tokio::test]
    async fn test_handle_socket_client_close() {
        let (addr, broadcast) = spawn_test_ws_server().await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("client connect failed");

        // Consume Connected
        tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for Connected")
            .unwrap()
            .unwrap();

        // Client sends close frame
        ws.close(None).await.unwrap();

        // Server should handle the close gracefully — sending after close
        // should not panic (the broadcast just goes nowhere).
        broadcast.send(store(), StatusUpdate::Ping);
    }

    /// Verify the exact JSON contract that the client crate relies on.
    /// Both server and client define identical `StatusUpdate` enums. This test
    /// asserts the canonical JSON so any serde-attribute drift is caught.
    #[test]
    fn test_status_update_json_contract() {
        // Connected
        assert_eq!(
            serde_json::to_string(&StatusUpdate::Connected).unwrap(),
            r#"{"type":"connected"}"#,
        );

        // Ping
        assert_eq!(
            serde_json::to_string(&StatusUpdate::Ping).unwrap(),
            r#"{"type":"ping"}"#,
        );

        // InvoiceStatus
        let invoice = StatusUpdate::InvoiceStatus {
            invoice_id: "inv_1".to_string(),
            status: "paid".to_string(),
        };
        let json = serde_json::to_value(&invoice).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "invoice_status",
                "invoice_id": "inv_1",
                "status": "paid"
            })
        );

        // PaymentUpdate with amount
        let payment = StatusUpdate::PaymentUpdate {
            payment_id: "pay_1".to_string(),
            invoice_id: "inv_1".to_string(),
            status: "confirmed".to_string(),
            amount: Some("1.5".to_string()),
        };
        let json = serde_json::to_value(&payment).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "payment_update",
                "payment_id": "pay_1",
                "invoice_id": "inv_1",
                "status": "confirmed",
                "amount": "1.5"
            })
        );

        // PaymentUpdate without amount
        let payment_no_amt = StatusUpdate::PaymentUpdate {
            payment_id: "pay_2".to_string(),
            invoice_id: "inv_2".to_string(),
            status: "detecting".to_string(),
            amount: None,
        };
        let json = serde_json::to_value(&payment_no_amt).unwrap();
        // Use .get().unwrap().is_null() instead of json["amount"] == Null
        // so this assertion catches a future skip_serializing_if annotation
        // that would omit the key entirely.
        assert!(json.get("amount").unwrap().is_null());
    }
}
