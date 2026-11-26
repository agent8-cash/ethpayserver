//! Invoice list concurrency scenario — read-heavy load test.
//!
//! Target: 20 concurrent queries without connection-wait > 50ms p95
//! (Postgres pool saturation test).

use goose::prelude::*;

use crate::session::Session;

/// Goose transaction: GET /invoices with pagination.
pub async fn list_invoices(user: &mut GooseUser) -> TransactionResult {
    let session = user.get_session_data::<Session>().unwrap().clone();

    let limit = 20;
    let offset = (rand::random::<u32>() % 5) * limit;
    let path = format!(
        "/api/invoices?store_id={}&limit={limit}&offset={offset}",
        session.store_id,
    );

    let builder = user
        .get_request_builder(&GooseMethod::Get, &path)?
        .header("Authorization", format!("Bearer {}", session.api_key));

    let goose_request = GooseRequest::builder()
        .set_request_builder(builder)
        .expect_status_code(200)
        .name("GET /invoices")
        .build();

    user.request(goose_request).await?;
    Ok(())
}
