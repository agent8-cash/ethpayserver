//! Webhook burst scenario — measures invoice-creation throughput when every
//! invoice carries a webhook URL, stressing the webhook queue insertion path.
//!
//! Target: 100 deliveries/min sustained without growing queue depth.
//!
//! Note: actual webhook *delivery* requires invoices to transition through
//! payment states. This scenario measures the write-path overhead of webhook-
//! enabled invoices. Full delivery throughput should be validated with the
//! end-to-end test harness against a running webhook receiver.

use goose::prelude::*;

use crate::session::Session;

/// Goose transaction: POST /invoices with a webhook_url attached.
pub async fn create_invoice_with_webhook(user: &mut GooseUser) -> TransactionResult {
    let session = user.get_session_data::<Session>().unwrap().clone();

    let amount = format!("{:.2}", 1.0 + rand::random::<f64>() * 999.0);
    let body = serde_json::json!({
        "store_id": session.store_id,
        "currency": "USD",
        "amount": amount,
        "expiration_seconds": 900,
        "webhook_url": session.webhook_receiver_url,
    });

    let builder = user
        .get_request_builder(&GooseMethod::Post, "/api/invoices")?
        .header("Authorization", format!("Bearer {}", session.api_key))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body).unwrap());

    let goose_request = GooseRequest::builder()
        .set_request_builder(builder)
        .expect_status_code(201)
        .name("POST /invoices (webhook)")
        .build();

    user.request(goose_request).await?;
    Ok(())
}
