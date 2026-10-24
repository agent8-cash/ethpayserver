//! `ServerSettingsRepository`, against a real database.
//!
//! These exist because the read path was broken from the day the
//! `enabled_chain_ids` column became a `caip2[]` domain, and nothing caught
//! it. The column is an array of a DOMAIN over text; sqlx decodes by type
//! OID, so asking for `Vec<String>` off a `caip2[]` fails every time, whatever
//! the values are.
//!
//! It stayed invisible because no settings row ever existed on any instance:
//! `fetch_optional` returned `None`, the decode never ran, and every caller
//! fell back to `ServerSettings::default()`. The first successful save would
//! have written a row and then crash-looped the process on the next boot,
//! since the decode was an unwrapping `get` in a value read during startup.
//!
//! A unit test could not have found it - there is no `caip2` domain outside a
//! real database - which is exactly why these live here.
//!
//! SECOND OCCURRENCE, 2026-10-24: a `server/src/api/admin` test asserted the
//! saved value with its own `sqlx::query_scalar::<_, serde_json::Value>(...)`
//! instead of through `ServerSettingsRepository`, hit the same decode
//! mismatch, and failed - while the handler it was testing, which does go
//! through the repository, was correct. **Integration tests assert through
//! the repository, never raw SQL, unless testing the SQL itself is the
//! point** - a test with its own query exercises a path production never
//! takes, so a decode bug lives exactly where nothing is checking.

use crate::test_support::pg_service;
use types::StoreId;
use uuid::Uuid;

use crate::postgres::PgDataService;
use auth::{ServerSettings, ServerSettingsRepository};

/// Put the settings table back to having no row.
///
/// Not tidiness - these tests share one database with every other
/// integration test, and a settings row changes behaviour elsewhere. With no
/// row, `chain_has_no_adapter` falls back to the compiled-in chain configs
/// and accepts Sepolia; with one, it accepts only what `enabled_chain_ids`
/// lists. Leaving a row behind therefore breaks
/// `a_sepolia_payment_method_is_still_created_by_the_handler` in a different
/// crate, which is exactly what it did before this existed.
///
/// Run at the start as well as the end: a previous run that panicked
/// mid-test leaves its row behind, and the next run should not inherit it.
async fn without_a_settings_row(service: &PgDataService) {
    sqlx::query("DELETE FROM server_settings WHERE id = 1")
        .execute(service.pool())
        .await
        .expect("clearing the settings row");
}

fn settings_with(chains: Vec<types::ChainId>, store: Option<StoreId>) -> ServerSettings {
    ServerSettings {
        default_confirmations: 5,
        invoice_expiry_minutes: 45,
        rate_limit_rpm: 250,
        enabled_chain_ids: chains,
        operator_store_id: store,
    }
}

/// The regression. Write a row, read it back - which is the sequence that
/// never worked, and which no amount of reading with no row present would
/// have exercised.
#[tokio::test]
#[ignore]
async fn a_written_settings_row_can_be_read_back() {
    let service = pg_service().await;
    without_a_settings_row(&service).await;

    let chains = vec![types::ChainId::evm(1), types::ChainId::evm(11155111)];
    let store = StoreId(Uuid::new_v4());
    service
        .upsert_server_settings(&settings_with(chains.clone(), Some(store)))
        .await
        .expect("settings should save");

    let read = service
        .get_server_settings()
        .await
        .expect("reading settings must not fail")
        .expect("the row just written must be there");

    assert_eq!(read.default_confirmations, 5);
    assert_eq!(read.invoice_expiry_minutes, 45);
    assert_eq!(read.rate_limit_rpm, 250);
    assert_eq!(
        read.enabled_chain_ids, chains,
        "the chain ids must survive the caip2[] column intact"
    );
    assert_eq!(read.operator_store_id, Some(store));

    without_a_settings_row(&service).await;
}

/// Clearing the operator store is how an instance stops invoicing itself, and
/// it has to be distinguishable from never having set one.
#[tokio::test]
#[ignore]
async fn the_operator_store_can_be_set_and_cleared() {
    let service = pg_service().await;
    without_a_settings_row(&service).await;

    let store = StoreId(Uuid::new_v4());
    service
        .upsert_server_settings(&settings_with(vec![types::ChainId::evm(1)], Some(store)))
        .await
        .expect("save with a store");
    assert_eq!(
        service
            .get_server_settings()
            .await
            .unwrap()
            .unwrap()
            .operator_store_id,
        Some(store)
    );

    service
        .upsert_server_settings(&settings_with(vec![types::ChainId::evm(1)], None))
        .await
        .expect("save without a store");
    assert_eq!(
        service
            .get_server_settings()
            .await
            .unwrap()
            .unwrap()
            .operator_store_id,
        None,
        "clearing must actually clear, not leave the previous store in place"
    );

    without_a_settings_row(&service).await;
}

/// An empty list is a real answer - a server that enables no chains from
/// settings - and must not be confused with a decode that gave up.
#[tokio::test]
#[ignore]
async fn no_enabled_chains_round_trips_as_an_empty_list() {
    let service = pg_service().await;
    without_a_settings_row(&service).await;

    service
        .upsert_server_settings(&settings_with(Vec::new(), None))
        .await
        .expect("save with no chains");

    let read = service.get_server_settings().await.unwrap().unwrap();
    assert!(read.enabled_chain_ids.is_empty());

    without_a_settings_row(&service).await;
}
