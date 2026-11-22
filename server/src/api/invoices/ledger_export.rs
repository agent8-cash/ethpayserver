//! Ledger export: one row per received payment, with the scale and the fiat
//! value that the plain payments CSV leaves out.
//!
//! The payments CSV is an operational list — raw smallest-unit amounts, no
//! asset scale, no quoted rate, and `to_address`/`paid_at` written as empty
//! strings. That is enough to watch money arrive and not enough to hand to an
//! accountant: `1500000` is 1.5 USDC, 0.015 USDC or 1500000 of an 8-decimal
//! token depending on the asset, and the fiat value of a crypto receipt is
//! what a tax filing needs.
//!
//! Every column here comes from data that is already persisted:
//!
//! * `PaymentData` carries the asset symbol, the token address, the raw amount
//!   and the credited amount in the invoice currency, plus the locked rate that
//!   produced it.
//! * `PaymentOptionData` carries the decimals and the exact address the payment
//!   was sent to.
//! * `InvoiceData` carries the invoice currency and what was asked for, which
//!   is the other half of an over/under comparison.
//!
//! Nothing is guessed. When a payment has no payment option (or an option with
//! no recorded rate) the affected cells stay **empty**, not zero: an empty cell
//! says "not recorded", and a zero would be a claim about the money.
//!
//! `credited_amount` follows the same convention the invoice totals follow:
//! `None` means the payment is recorded for audit but does **not** count toward
//! the invoice, so `credit_state` says `not_counted` rather than leaving the
//! reader to infer it from an empty amount.

use axum::{
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::Response,
};
use futures::StreamExt;
use std::sync::Arc;

use ::types::{
    InvoiceData, InvoiceReader, PaymentData, PaymentOptionData, PaymentOptionId,
    PaymentOptionReader, PaymentReader,
};
use auth::SessionService;

use super::{
    ListPaymentsQuery, StoreScope, VIEW_INVOICES, narrow_scope_by_key,
    verify_store_access_for_query,
};
use crate::api::ApiErr;
use crate::api::extractors::StoreScopedUser;
use crate::api::invoices::csv_export::{
    EXPORT_PAGE_SIZE, MAX_EXPORT_ROWS, build_payment_filter_params, csv_row,
};
use crate::state::PgAppState;

/// Ledger columns, in output order.
///
/// A settlement event is one payment. The invoice hangs off it, so every row is
/// self-contained: an accountant can group by invoice id without a second file.
pub(crate) const LEDGER_HEADER: &[&str] = &[
    "payment_id",
    "payment_option_id",
    "invoice_id",
    "order_number",
    "customer_email",
    "chain_id",
    "asset_symbol",
    "asset_type",
    "token_address",
    "amount_base_units",
    "decimals",
    "amount_decimal",
    "tx_hash",
    "block_number",
    "from_address",
    "to_address",
    "status",
    "detected_at",
    "confirmed_at",
    "invoice_currency",
    "invoice_amount",
    "invoice_amount_received",
    "rate_used",
    "rate_applied_at",
    "fiat_value_credited",
    "credit_state",
];

/// Render a smallest-unit amount as an exact decimal string.
///
/// Pure integer string arithmetic on purpose: the value is a `NUMERIC(78,0)`
/// and is routinely far past what an `f64` represents exactly, so parsing it
/// into a float would quietly change the number an accountant books. `None`
/// when the amount is not a plain run of digits, because then the true scale is
/// not knowable and an empty cell is honest where a fabricated one is not.
pub(crate) fn base_units_to_decimal(amount: &str, decimals: u8) -> Option<String> {
    let digits = amount.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = decimals as usize;
    if scale == 0 {
        return Some(digits.to_string());
    }
    // Strip leading zeros so the split below always has a stable shape, but
    // keep a lone "0" so "0" formats as 0.000000 rather than ".000000".
    let trimmed = digits.trim_start_matches('0');
    let digits = if trimmed.is_empty() { "0" } else { trimmed };

    let (whole, frac) = if digits.len() > scale {
        let split = digits.len() - scale;
        (&digits[..split], &digits[split..])
    } else {
        ("0", digits)
    };
    let padded = format!("{frac:0>width$}", width = scale);
    Some(format!("{whole}.{padded}"))
}

/// One ledger row, already stringified and in `LEDGER_HEADER` order.
///
/// Split out from the streaming handler so the mapping can be tested against
/// fixtures without a database, and so a wrong column order fails a unit test
/// rather than a merchant's spreadsheet.
pub(crate) fn build_ledger_row(
    payment: &PaymentData,
    invoice: Option<&InvoiceData>,
    option: Option<&PaymentOptionData>,
) -> Vec<String> {
    let order_number = invoice
        .and_then(|i| i.metadata.as_ref())
        .and_then(|m| m.get("order_number"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let status = if payment.reorged {
        "reorged"
    } else if payment.confirmed_at.is_some() {
        "confirmed"
    } else {
        "pending"
    };

    let decimals = option.map(|o| o.decimals.to_string()).unwrap_or_default();
    let amount_decimal = option
        .and_then(|o| base_units_to_decimal(&payment.amount, o.decimals))
        .unwrap_or_default();

    let (fiat_value, credit_state) = match &payment.credited_amount {
        Some(credited) => (credited.clone(), "counted"),
        None => (String::new(), "not_counted"),
    };

    vec![
        payment.id.to_string(),
        payment
            .payment_option_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        payment.invoice_id.0.to_string(),
        order_number.to_string(),
        invoice
            .and_then(|i| i.customer_email.clone())
            .unwrap_or_default(),
        payment.chain_id.to_string(),
        payment.asset_symbol.clone(),
        payment.asset_type.to_string(),
        payment.token_address.clone().unwrap_or_default(),
        payment.amount.clone(),
        decimals,
        amount_decimal,
        payment.tx_hash.clone(),
        payment
            .block_number
            .map(|n| n.to_string())
            .unwrap_or_default(),
        payment.from_address.clone().unwrap_or_default(),
        option
            .map(|o| o.payment_address.clone())
            .unwrap_or_default(),
        status.to_string(),
        payment
            .detected_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        payment
            .confirmed_at
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default(),
        invoice.map(|i| i.currency.clone()).unwrap_or_default(),
        invoice.map(|i| i.amount.clone()).unwrap_or_default(),
        invoice
            .map(|i| i.amount_received.clone())
            .unwrap_or_default(),
        payment.rate_used.clone().unwrap_or_default(),
        payment
            .rate_applied_at
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default(),
        fiat_value,
        credit_state.to_string(),
    ]
}

fn header_row() -> String {
    csv_row(LEDGER_HEADER)
}

/// Columns whose value is free text a payer can influence.
///
/// These are the columns a spreadsheet will evaluate. `order_number` comes from
/// invoice metadata and `customer_email` from whoever paid, so both are
/// attacker-controlled by the person being invoiced.
const PAYER_INFLUENCED_TEXT_COLUMNS: &[&str] = &["order_number", "customer_email"];

/// Neutralise a leading spreadsheet formula trigger.
///
/// Quoting is not enough: Excel and LibreOffice still evaluate a quoted cell
/// that begins with `=`, `+`, `-` or `@`, and also treat a leading TAB or CR as
/// a prefix to one. A merchant opening this file in a spreadsheet is the whole
/// point of the file, so a payer-supplied value must never become a formula -
/// `=HYPERLINK(...)` in an order number would otherwise run when the accountant
/// opens the ledger. Prefixing an apostrophe makes the cell literal text.
///
/// The value is only touched when the first character is a trigger, so ordinary
/// addresses, ids and amounts stay byte-identical to what they were.
fn neutralise_formula(field: &str) -> String {
    match field.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{field}"),
        _ => field.to_string(),
    }
}

/// Apply `neutralise_formula` to the payer-influenced columns of a built row.
///
/// Kept separate from `build_ledger_row` so the neutralisation is a deliberate,
/// column-scoped step rather than being hidden inside a 26-element vector
/// literal, where a later column reorder would silently drop it.
fn neutralise_row(row: &mut [String]) {
    for name in PAYER_INFLUENCED_TEXT_COLUMNS {
        if let Some(i) = LEDGER_HEADER.iter().position(|h| h == name)
            && let Some(cell) = row.get_mut(i)
        {
            *cell = neutralise_formula(cell);
        }
    }
}

/// Render one page of payments as CSV rows.
///
/// `Err(())` means the data service failed, which ends the stream the same way
/// a failed page read does - half a ledger is worse than an error, because the
/// truncated file still opens and still reconciles against nothing.
async fn page_to_csv<D>(ds: &D, payments: &[PaymentData]) -> Result<String, ()>
where
    D: InvoiceReader + PaymentOptionReader,
{
    let mut chunk = String::new();
    for payment in payments {
        // A payment whose invoice is gone is still a real receipt: the
        // invoice-side columns stay empty rather than dropping the row or
        // inventing an invoice to fill them.
        let invoice = InvoiceReader::get(ds, &payment.invoice_id)
            .await
            .map_err(|_| ())?;

        // A failed option read must not become "no option": empty scale and
        // destination cells are indistinguishable from a payment that never had
        // an option, and the accountant would then read a real payment as one
        // that cannot be scaled. Same rule as the invoice read above.
        let option: Option<PaymentOptionData> = match payment.payment_option_id {
            Some(id) => PaymentOptionReader::get(ds, &PaymentOptionId(id))
                .await
                .map_err(|_| ())?,
            None => None,
        };

        let mut row = build_ledger_row(payment, invoice.as_ref(), option.as_ref());
        neutralise_row(&mut row);
        let refs: Vec<&str> = row.iter().map(String::as_str).collect();
        chunk.push_str(&csv_row(&refs));
    }
    Ok(chunk)
}

/// Export a payments ledger as a streaming CSV file.
///
/// Same store scoping and filters as `list_payments`, so the file matches what
/// the merchant is looking at. Each page of payments is joined to its invoice
/// and payment option by id; a missing option leaves the scale columns empty
/// rather than blocking the row, because a payment that cannot be scaled is
/// exactly the row a bookkeeper needs to see.
pub async fn export_ledger_csv<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Query(query): Query<ListPaymentsQuery>,
) -> Result<Response, ApiErr>
where
    A: SessionService + 'static,
{
    let scope = verify_store_access_for_query(&*state.data_service, &user, query.store_id).await?;
    let scope = narrow_scope_by_key(scope, key_scope.as_deref(), VIEW_INVOICES)?;
    let base_params =
        build_payment_filter_params(&scope, query.status.as_deref(), query.search.as_deref())?;

    let count_params = base_params.clone().with_limit(1).with_offset(0);
    let (total, _) = PaymentReader::query(&*state.data_service, &count_params)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if total > MAX_EXPORT_ROWS {
        let body = serde_json::json!({
            "error": "export_too_large",
            "max_rows": MAX_EXPORT_ROWS,
            "matched_rows": total,
        });
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into());
    }

    let store_label = match &scope {
        StoreScope::One(s) => s.0.to_string(),
        _ => "all".to_string(),
    };
    let date = chrono::Utc::now().format("%Y%m%d");
    let filename = format!("ledger_{}_{}.csv", store_label, date);

    // The body is a fallible stream on purpose: a mid-export failure has to
    // reach the client as an aborted download. A cleanly-ended 200 would look
    // like a complete ledger that is silently short of rows.
    let header_stream = futures::stream::once(async { Ok::<_, std::io::Error>(header_row()) });

    let ds = Arc::clone(&state.data_service);
    let data_stream =
        futures::stream::unfold((0i64, base_params, ds), |(offset, params, ds)| async move {
            let page = params
                .clone()
                .with_limit(EXPORT_PAGE_SIZE)
                .with_offset(offset);
            // Both failure paths yield a stream error, not an end: the client
            // sees an aborted download instead of a 200 with rows missing.
            let payments = match PaymentReader::query(&*ds, &page).await {
                Ok((_, rows)) => rows,
                Err(_) => {
                    return Some((
                        Err(std::io::Error::other("payment page read failed")),
                        (offset, params, ds),
                    ));
                }
            };
            if payments.is_empty() {
                return None;
            }

            let new_offset = offset + payments.len() as i64;
            let chunk = match page_to_csv(&*ds, &payments).await {
                Ok(chunk) => chunk,
                Err(()) => {
                    return Some((
                        Err(std::io::Error::other("ledger join failed")),
                        (new_offset, params, ds),
                    ));
                }
            };
            Some((Ok::<_, std::io::Error>(chunk), (new_offset, params, ds)))
        });

    let body = Body::from_stream(header_stream.chain(data_stream));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/csv; charset=utf-8")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Duration;
    use types::{AssetType, ChainId, InvoiceId, InvoiceStatus};

    fn invoice() -> InvoiceData {
        InvoiceData {
            id: InvoiceId::new(),
            store_id: Default::default(),
            currency: "USD".to_string(),
            status: InvoiceStatus::Pending,
            amount: "100.00".to_string(),
            amount_received: "100.00".to_string(),
            created_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + Duration::hours(1),
            metadata: Some(serde_json::json!({"order_number": "ORD-7"})),
            customer_email: Some("buyer@example.com".to_string()),
            extra: None,
        }
    }

    fn payment(amount: &str, credited: Option<&str>) -> PaymentData {
        PaymentData {
            id: uuid::Uuid::new_v4(),
            invoice_id: InvoiceId::new(),
            payment_option_id: Some(uuid::Uuid::new_v4()),
            chain_id: ChainId::parse("eip155:8453").unwrap(),
            asset_type: AssetType::ERC20,
            amount: amount.to_string(),
            asset_symbol: "USDC".to_string(),
            token_address: Some("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_string()),
            tx_hash: "0xabc".to_string(),
            block_number: Some(42),
            detected_at: chrono::Utc::now(),
            confirmed_at: Some(chrono::Utc::now()),
            from_address: Some("0xsender".to_string()),
            reorged: false,
            extra: None,
            credited_amount: credited.map(ToString::to_string),
            rate_used: Some("1.0".to_string()),
            rate_applied_at: Some(chrono::Utc::now()),
        }
    }

    fn option(decimals: u8) -> PaymentOptionData {
        let mut o = PaymentOptionData::erc20(
            InvoiceId::new(),
            ChainId::parse("eip155:8453").unwrap(),
            "USDC",
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            decimals,
            "0xmerchant",
            "1500000",
        );
        o.rate = Some("1.0".to_string());
        o
    }

    #[test]
    fn header_and_row_have_the_same_width() {
        let row = build_ledger_row(
            &payment("1500000", Some("1.50")),
            Some(&invoice()),
            Some(&option(6)),
        );
        assert_eq!(row.len(), LEDGER_HEADER.len());
    }

    #[test]
    fn six_decimals_renders_one_point_five() {
        assert_eq!(
            base_units_to_decimal("1500000", 6).as_deref(),
            Some("1.500000")
        );
    }

    #[test]
    fn eighteen_decimals_stays_exact_past_f64_precision() {
        // 1.000000000000000001 ETH: an f64 would round this to 1.0.
        assert_eq!(
            base_units_to_decimal("1000000000000000001", 18).as_deref(),
            Some("1.000000000000000001")
        );
    }

    #[test]
    fn amount_exactly_at_the_scale_boundary_keeps_a_leading_zero() {
        // Six digits at six decimals is 0.123456, not .123456: the whole part
        // is zero-valued but must still be written. This is the one length
        // where "> scale" and ">= scale" disagree, so without it a regression
        // in that comparison passes every other case.
        assert_eq!(
            base_units_to_decimal("123456", 6).as_deref(),
            Some("0.123456")
        );
        assert_eq!(
            base_units_to_decimal("1234567", 6).as_deref(),
            Some("1.234567")
        );
    }

    #[test]
    fn dust_below_one_unit_keeps_a_leading_zero() {
        assert_eq!(base_units_to_decimal("1", 6).as_deref(), Some("0.000001"));
        assert_eq!(base_units_to_decimal("0", 6).as_deref(), Some("0.000000"));
    }

    #[test]
    fn zero_decimals_and_non_digits_do_not_invent_scale() {
        assert_eq!(base_units_to_decimal("42", 0).as_deref(), Some("42"));
        assert_eq!(base_units_to_decimal("1.5", 6), None);
        assert_eq!(base_units_to_decimal("", 6), None);
    }

    #[test]
    fn missing_option_leaves_scale_and_destination_empty() {
        let row = build_ledger_row(&payment("1500000", Some("1.50")), Some(&invoice()), None);
        let col = |name: &str| -> String {
            let i = LEDGER_HEADER.iter().position(|h| *h == name).unwrap();
            row[i].clone()
        };
        assert_eq!(col("decimals"), "");
        assert_eq!(col("amount_decimal"), "");
        assert_eq!(col("to_address"), "");
        // The asset symbol is on the payment and must survive without an option.
        assert_eq!(col("asset_symbol"), "USDC");
    }

    #[test]
    fn credited_amount_missing_is_not_counted_not_zero() {
        let row = build_ledger_row(
            &payment("1500000", None),
            Some(&invoice()),
            Some(&option(6)),
        );
        let i = LEDGER_HEADER
            .iter()
            .position(|h| *h == "fiat_value_credited")
            .unwrap();
        let s = LEDGER_HEADER
            .iter()
            .position(|h| *h == "credit_state")
            .unwrap();
        assert_eq!(row[i], "");
        assert_eq!(row[s], "not_counted");
    }

    #[test]
    fn address_and_confirm_time_are_filled_from_the_option_and_payment() {
        let p = payment("1500000", Some("1.50"));
        let row = build_ledger_row(&p, Some(&invoice()), Some(&option(6)));
        let col = |name: &str| -> String {
            let i = LEDGER_HEADER.iter().position(|h| *h == name).unwrap();
            row[i].clone()
        };
        assert_eq!(col("to_address"), "0xmerchant");
        assert_eq!(col("chain_id"), "eip155:8453");
        assert_eq!(col("order_number"), "ORD-7");
        assert_eq!(col("status"), "confirmed");
        assert!(!col("confirmed_at").is_empty());
    }

    /// A data service that answers the two joins `page_to_csv` needs and can be
    /// told to fail either one, so the fail-closed behaviour is exercised
    /// without a database.
    struct JoinStub {
        invoice: Option<InvoiceData>,
        option: Option<PaymentOptionData>,
        invoice_error: bool,
        option_error: bool,
    }

    #[async_trait::async_trait]
    impl InvoiceReader for JoinStub {
        async fn get(&self, _: &InvoiceId) -> types::RepositoryResult<Option<InvoiceData>> {
            if self.invoice_error {
                return Err(types::RepositoryError::Database("boom".to_string()));
            }
            Ok(self.invoice.clone())
        }
        async fn query(
            &self,
            _: &types::InvoiceQueryParams,
        ) -> types::RepositoryResult<(i64, Vec<InvoiceData>)> {
            unimplemented!("page_to_csv reads rows through PaymentReader, not InvoiceReader")
        }
        async fn get_expired(&self) -> types::RepositoryResult<Vec<InvoiceData>> {
            unimplemented!("not exercised here")
        }
        fn stream_expired_pending(
            &self,
        ) -> futures::stream::BoxStream<'_, types::RepositoryResult<InvoiceId>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[async_trait::async_trait]
    impl PaymentOptionReader for JoinStub {
        async fn get(
            &self,
            _: &PaymentOptionId,
        ) -> types::RepositoryResult<Option<PaymentOptionData>> {
            if self.option_error {
                return Err(types::RepositoryError::Database("boom".to_string()));
            }
            Ok(self.option.clone())
        }
        async fn get_for_invoice(
            &self,
            _: &InvoiceId,
        ) -> types::RepositoryResult<Vec<PaymentOptionData>> {
            unimplemented!("not exercised here")
        }
        async fn get_by_payment_method(
            &self,
            _: &InvoiceId,
            _: &types::PaymentMethodId,
        ) -> types::RepositoryResult<Option<PaymentOptionData>> {
            unimplemented!("not exercised here")
        }
        async fn get_active_for_invoice(
            &self,
            _: &InvoiceId,
        ) -> types::RepositoryResult<Vec<PaymentOptionData>> {
            unimplemented!("not exercised here")
        }
        async fn get_by_address(
            &self,
            _: &str,
            _: &ChainId,
            _: Option<&str>,
        ) -> types::RepositoryResult<Option<PaymentOptionData>> {
            unimplemented!("not exercised here")
        }
    }

    #[tokio::test]
    async fn failed_invoice_join_fails_the_page_instead_of_dropping_the_row() {
        let stub = JoinStub {
            invoice: None,
            option: Some(option(6)),
            invoice_error: true,
            option_error: false,
        };
        let p = payment("1500000", Some("1.50"));
        assert!(
            page_to_csv(&stub, std::slice::from_ref(&p)).await.is_err(),
            "a failed invoice read must end the stream as an error"
        );
    }

    #[tokio::test]
    async fn failed_option_join_is_not_read_as_a_payment_without_an_option() {
        // The row renders fine with no option - that is the bug. A database
        // error has to be distinguishable from "this payment has no option
        // row", otherwise a real payment exports with empty scale/address as if
        // the data were simply absent.
        let stub = JoinStub {
            invoice: Some(invoice()),
            option: None,
            invoice_error: false,
            option_error: true,
        };
        let p = payment("1500000", Some("1.50"));
        assert!(
            page_to_csv(&stub, std::slice::from_ref(&p)).await.is_err(),
            "a failed option read must not be swallowed into an empty column"
        );
    }

    #[tokio::test]
    async fn a_missing_option_row_still_renders_the_payment() {
        // The control for the test above: an option that is genuinely absent
        // must still produce a row, with the payment-level columns filled.
        let stub = JoinStub {
            invoice: Some(invoice()),
            option: None,
            invoice_error: false,
            option_error: false,
        };
        let p = payment("1500000", Some("1.50"));
        let csv = page_to_csv(&stub, std::slice::from_ref(&p))
            .await
            .expect("an absent option is not an error");
        assert_eq!(csv.matches("\r\n").count(), 1);
        assert!(csv.contains("USDC"));
    }

    #[tokio::test]
    async fn rows_from_page_to_csv_are_formula_neutralised() {
        let mut inv = invoice();
        inv.metadata = Some(serde_json::json!({"order_number": "=1+1"}));
        let stub = JoinStub {
            invoice: Some(inv),
            option: Some(option(6)),
            invoice_error: false,
            option_error: false,
        };
        let p = payment("1500000", Some("1.50"));
        let csv = page_to_csv(&stub, std::slice::from_ref(&p))
            .await
            .expect("happy path");
        // Quoted by the CSV escaper because of the apostrophe? No: an
        // apostrophe is not special, so the cell stays unquoted and must not
        // begin the field with '='.
        assert!(csv.contains("'=1+1"), "got: {csv}");
        assert!(!csv.contains(",=1+1"), "a bare formula survived: {csv}");
    }

    #[test]
    fn payer_supplied_text_cannot_become_a_spreadsheet_formula() {
        // An order number and a customer email are both written by the payer.
        // A leading =, +, -, @, TAB or CR makes Excel and LibreOffice treat the
        // cell as a formula, so each one must be prefixed with an apostrophe.
        let mut inv = invoice();
        inv.metadata = Some(serde_json::json!({"order_number": "=HYPERLINK(\"http://x\",\"a\")"}));
        inv.customer_email = Some("@evil.example".to_string());

        let mut row = build_ledger_row(
            &payment("1500000", Some("1.50")),
            Some(&inv),
            Some(&option(6)),
        );
        neutralise_row(&mut row);

        let col = |name: &str| -> String {
            let i = LEDGER_HEADER.iter().position(|h| *h == name).unwrap();
            row[i].clone()
        };
        assert_eq!(col("order_number"), "'=HYPERLINK(\"http://x\",\"a\")");
        assert_eq!(col("customer_email"), "'@evil.example");
    }

    #[test]
    fn every_formula_trigger_is_neutralised_and_ordinary_text_is_not() {
        for trigger in ["=cmd", "+cmd", "-cmd", "@cmd", "\tcmd", "\rcmd"] {
            assert_eq!(
                neutralise_formula(trigger),
                format!("'{trigger}"),
                "{trigger:?} would be evaluated by a spreadsheet"
            );
        }
        // A negative-looking but non-leading trigger is ordinary text.
        assert_eq!(neutralise_formula("A-1"), "A-1");
        assert_eq!(neutralise_formula("buyer@example.com"), "buyer@example.com");
        assert_eq!(neutralise_formula(""), "");
    }

    #[test]
    fn neutralisation_touches_only_the_payer_influenced_columns() {
        // The tx hash, addresses and amounts are produced by the chain reader,
        // not the payer, and must stay byte-identical - including a value that
        // starts with a trigger character.
        let mut p = payment("1500000", Some("-1.50"));
        p.tx_hash = "0xabc".to_string();
        let mut row = build_ledger_row(&p, Some(&invoice()), Some(&option(6)));
        let before = row.clone();
        neutralise_row(&mut row);

        let changed: Vec<usize> = before
            .iter()
            .zip(row.iter())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        assert!(
            changed.is_empty(),
            "no payer-influenced column in this row starts with a trigger, so nothing may change; changed indices {changed:?}"
        );

        // And the fiat column keeps its leading minus untouched.
        let i = LEDGER_HEADER
            .iter()
            .position(|h| *h == "fiat_value_credited")
            .unwrap();
        assert_eq!(row[i], "-1.50");
    }

    #[test]
    fn reorged_beats_confirmed_in_the_status_column() {
        let mut p = payment("1500000", Some("1.50"));
        p.reorged = true;
        let row = build_ledger_row(&p, Some(&invoice()), Some(&option(6)));
        let i = LEDGER_HEADER.iter().position(|h| *h == "status").unwrap();
        assert_eq!(row[i], "reorged");
    }
}
