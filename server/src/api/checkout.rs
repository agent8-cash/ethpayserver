//! Public checkout API endpoints.
//!
//! These endpoints are unauthenticated — any customer with an invoice ID
//! can view payment details. No store internals are exposed.

use axum::{
    Json,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;

use auth::AuthenticationService;
use data_service::{PaymentOptionReader, PaymentReader};
use types::{InvoiceId, InvoiceReader, InvoiceStatus};

use super::ws::StatusUpdate;
use crate::state::PgAppState;
pub use api_types::{CheckoutPaymentInfo, CheckoutResponse};

/// Get public checkout data for an invoice.
///
/// No authentication required. Returns payment-relevant fields only.
pub async fn get_checkout<A>(
    State(state): State<PgAppState<A>>,
    Path(invoice_id): Path<String>,
) -> Result<Json<CheckoutResponse>, StatusCode>
where
    A: AuthenticationService + 'static,
{
    let id = InvoiceId::from_string(invoice_id);

    let invoice = InvoiceReader::get(&*state.data_service, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let payments = PaymentReader::get_for_invoice(&*state.data_service, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let options = PaymentOptionReader::get_for_invoice(&*state.data_service, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let now = chrono::Utc::now();

    Ok(Json(CheckoutResponse {
        id: invoice.id.0,
        currency: invoice.currency,
        status: invoice.status.to_string(),
        amount: invoice.amount.clone(),
        amount_received: invoice.amount_received,
        expires_at: invoice.expires_at,
        is_expired: invoice.status == InvoiceStatus::Expired || invoice.expires_at < now,
        is_paid: invoice.status == InvoiceStatus::Paid,
        payment_options: options.into_iter().map(Into::into).collect(),
        payments: payments
            .into_iter()
            .filter(|p| !p.reorged)
            .map(CheckoutPaymentInfo::from)
            .collect(),
    }))
}

/// Query params for public checkout WebSocket.
#[derive(Debug, Deserialize)]
pub struct CheckoutWsQuery {
    /// Invoice ID to subscribe to.
    pub invoice_id: String,
}

/// Public WebSocket handler for checkout status updates.
///
/// No auth required. Subscribes to the specified invoice's own channel.
pub async fn checkout_ws_handler<A>(
    ws: WebSocketUpgrade,
    Query(query): Query<CheckoutWsQuery>,
    State(state): State<PgAppState<A>>,
) -> impl IntoResponse
where
    A: AuthenticationService + 'static,
{
    // Validate that the invoice exists
    let id = InvoiceId::from_string(query.invoice_id.clone());
    match InvoiceReader::get(&*state.data_service, &id).await {
        Ok(Some(_)) => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    }

    let Some(ws_broadcast) = state.ws_broadcast.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // Subscribe to this invoice's own channel: no other invoice's update is
    // ever routed to a public socket.
    let rx = ws_broadcast.subscribe_invoice(&query.invoice_id);

    ws.on_upgrade(move |socket| handle_checkout_socket(socket, rx))
        .into_response()
}

/// What an unauthenticated checkout socket may be sent: an invoice's status and
/// the fields the public checkout response already returns for a payment.
///
/// This is deliberately not `StatusUpdate`: knowing an invoice id buys that
/// invoice's status, not whatever a shared type gains later. A new field has to
/// be added here on purpose to reach a public socket. The wire format matches
/// the corresponding `StatusUpdate` variants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type")]
enum CheckoutFrame {
    #[serde(rename = "invoice_status")]
    InvoiceStatus { invoice_id: String, status: String },
    #[serde(rename = "payment_update")]
    PaymentUpdate {
        payment_id: String,
        invoice_id: String,
        status: String,
        amount: Option<String>,
    },
    #[serde(rename = "connected")]
    Connected,
}

impl CheckoutFrame {
    /// The public view of an update, or `None` for one that carries nothing
    /// about an invoice.
    fn from_update(update: StatusUpdate) -> Option<Self> {
        match update {
            StatusUpdate::InvoiceStatus { invoice_id, status } => {
                Some(Self::InvoiceStatus { invoice_id, status })
            }
            StatusUpdate::PaymentUpdate {
                payment_id,
                invoice_id,
                status,
                amount,
            } => Some(Self::PaymentUpdate {
                payment_id,
                invoice_id,
                status,
                amount,
            }),
            StatusUpdate::Connected | StatusUpdate::Ping => None,
        }
    }
}

/// Handle a public checkout WebSocket connection.
///
/// `rx` is the invoice's own channel, so everything on it is forwarded, but
/// only as the minimal `CheckoutFrame`.
async fn handle_checkout_socket(
    socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<StatusUpdate>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Send connected acknowledgement
    let Ok(connected) = serde_json::to_string(&CheckoutFrame::Connected) else {
        return;
    };
    if sender.send(Message::Text(connected.into())).await.is_err() {
        return;
    }

    let mut send_task = tokio::spawn(async move {
        loop {
            let update = match rx.recv().await {
                Ok(update) => update,
                // Dropped updates may include the payment confirmation and the
                // page cannot know it missed one, so close and let it resync.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Some(frame) = CheckoutFrame::from_update(update) else {
                continue;
            };
            let msg = match serde_json::to_string(&frame) {
                Ok(json) => json,
                Err(_) => continue,
            };
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Close(_) = msg {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use types::ChainId;

    #[test]
    fn checkout_frames_keep_the_wire_format_of_the_shared_updates() {
        let updates = [
            StatusUpdate::InvoiceStatus {
                invoice_id: "i".into(),
                status: "paid".into(),
            },
            StatusUpdate::PaymentUpdate {
                payment_id: "p".into(),
                invoice_id: "i".into(),
                status: "confirmed".into(),
                amount: Some("5".into()),
            },
        ];
        for update in updates {
            let shared = serde_json::to_value(&update).unwrap();
            let frame = CheckoutFrame::from_update(update).unwrap();
            assert_eq!(serde_json::to_value(&frame).unwrap(), shared);
        }
        assert_eq!(
            serde_json::to_value(CheckoutFrame::Connected).unwrap(),
            serde_json::to_value(StatusUpdate::Connected).unwrap()
        );
        assert!(CheckoutFrame::from_update(StatusUpdate::Ping).is_none());
    }

    #[test]
    fn test_checkout_response_serialization() {
        let resp = CheckoutResponse {
            id: "inv_123".to_string(),
            currency: "USD".to_string(),
            status: "pending".to_string(),
            amount: "100.00".to_string(),
            amount_received: "0".to_string(),
            expires_at: chrono::Utc::now(),
            is_expired: false,
            is_paid: false,
            payment_options: vec![],
            payments: vec![],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "inv_123");
        assert_eq!(json["currency"], "USD");
        assert_eq!(json["is_paid"], false);
    }

    /// Privacy invariant: the public checkout endpoint MUST NOT leak the
    /// sender's wallet address (`from_address`) or internal state
    /// (`reorged`). Anyone with the invoice link hits this endpoint — leaking
    /// `from_address` would correlate a public invoice to the customer's
    /// wallet. Guard against a future refactor re-introducing the leak.
    #[test]
    fn test_checkout_payment_info_never_exposes_sender() {
        let info = CheckoutPaymentInfo {
            id: "pay_1".to_string(),
            chain_id: ChainId::parse("eip155:1").unwrap(),
            tx_hash: "0xabc".to_string(),
            amount: "1".to_string(),
            asset_symbol: "ETH".to_string(),
            token_address: None,
            block_number: Some(100),
            detected_at: chrono::Utc::now(),
            confirmed_at: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        let obj = json.as_object().unwrap();
        assert!(
            !obj.contains_key("from_address"),
            "from_address must not be in public checkout response"
        );
        assert!(
            !obj.contains_key("reorged"),
            "reorged is internal state, must not leak to checkout"
        );
    }

    #[test]
    fn test_checkout_ws_query_deserialize() {
        let json = serde_json::json!({ "invoice_id": "inv_abc" });
        let query: CheckoutWsQuery = serde_json::from_value(json).unwrap();
        assert_eq!(query.invoice_id, "inv_abc");
    }

    /// Ensures `GET /checkout/ws` routes to the WebSocket handler, not to
    /// `get_checkout` with `invoice_id = "ws"`. Axum prioritises static path
    /// segments over dynamic params, but this test guards against a future
    /// routing regression or a rebase that flips the order.
    #[tokio::test]
    async fn test_ws_route_wins_over_invoice_id_param() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        // Build a minimal router mirroring the production mount.
        let app: Router = Router::new()
            .route(
                "/{invoice_id}",
                axum::routing::get(
                    |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        format!("invoice={id}")
                    },
                ),
            )
            .route("/ws", axum::routing::get(|| async { "websocket" }));

        // A plain GET /ws (no upgrade headers) must hit the "websocket" handler,
        // not the `{invoice_id}` handler with id="ws".
        let resp = app
            .oneshot(Request::builder().uri("/ws").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"websocket");
    }
}
