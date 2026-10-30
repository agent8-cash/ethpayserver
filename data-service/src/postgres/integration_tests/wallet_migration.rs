//! The account-wallets migration, run against data in the old shape.
//!
//! Every other test here runs against the schema as it is now. This one is the
//! only place the *transition* is exercised, and the transition is where the
//! money is: the migration merges per-payment-method derivation counters onto
//! one counter per key, and a merge that lands too low re-issues an address a
//! customer has already been given.
//!
//! Each test builds its own database, applies every migration up to but not
//! including the one under test, seeds the old shape by hand, then applies it
//! and checks what survived.

use sqlx::{Executor, PgPool, Row};
use uuid::Uuid;

/// Filename stem of the migration under test.
const MIGRATION: &str = "20260908120000_account_wallets";

fn migrations_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres")
}

/// Read one migration file by stem.
pub(super) fn migration_sql(stem: &str) -> String {
    std::fs::read_to_string(migrations_dir().join(format!("{stem}.sql")))
        .unwrap_or_else(|e| panic!("read migration {stem}: {e}"))
}

/// Split a `DATABASE_URL` into (server url, database name).
fn split_url(url: &str) -> (String, String) {
    let cut = url.rfind('/').expect("DATABASE_URL has a database path");
    let db = url[cut + 1..].split('?').next().unwrap().to_string();
    (format!("{}/postgres", &url[..cut]), db)
}

/// Create a throwaway database and apply every migration *before* the one
/// under test.
///
/// Panics when `DATABASE_URL` is unset or unreachable, like the other
/// integration tests.
pub(super) async fn pre_migration_db(suffix: &str) -> (PgPool, String, String) {
    pre_migration_db_for(MIGRATION, suffix).await
}

/// The same, stopping before an arbitrary migration - so a later migration can
/// be exercised against the state a deployed database is actually in when it
/// runs, rather than against today's schema.
pub(super) async fn pre_migration_db_for(
    migration: &str,
    suffix: &str,
) -> (PgPool, String, String) {
    let url = crate::test_support::database_url();
    let (server_url, base) = split_url(&url);
    let name = format!("{base}_wallets_{suffix}");

    let admin = crate::test_support::pool_for(&server_url, 5).await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#).as_str())
        .await
        .expect("drop scratch database");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create scratch database");
    admin.close().await;

    let scratch_url = format!("{}/{name}", server_url.trim_end_matches("/postgres"));
    let pool = PgPool::connect(&scratch_url)
        .await
        .expect("connect scratch");

    // Apply migrations in filename order, stopping before the one under test.
    // Sorting by stem is exactly what sqlx does, so this reproduces the state a
    // deployed database is in the instant before the migration runs.
    let mut stems: Vec<String> = std::fs::read_dir(migrations_dir())
        .expect("read migrations dir")
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".sql")?;
            (!stem.ends_with(".down")).then(|| stem.to_string())
        })
        .collect();
    stems.sort();

    for stem in stems.iter().take_while(|s| s.as_str() != migration) {
        pool.execute(migration_sql(stem).as_str())
            .await
            .unwrap_or_else(|e| panic!("apply {stem}: {e}"));
    }

    (pool, name, server_url)
}

/// A throwaway database with EVERY migration applied.
///
/// The stem sorts after every real migration, so the `take_while` above runs
/// out of files rather than stopping early - which is how a caller asks for
/// "today's schema, in a database nobody else is writing to". Needed by any test
/// whose subject reads the WHOLE of a table (`SELECT ... FROM users` with no
/// user id), because on the shared test database another test's rows decide the
/// answer.
pub(super) async fn fully_migrated_db(suffix: &str) -> (PgPool, String, String) {
    pre_migration_db_for("zzzzzzzz_after_every_migration", suffix).await
}

pub(super) async fn drop_db(pool: PgPool, name: &str, server_url: &str) {
    pool.close().await;
    let admin = PgPool::connect(server_url).await.expect("connect admin");
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#).as_str())
        .await
        .expect("drop scratch database");
    admin.close().await;
}

/// Seed a user and a store, old-shape.
pub(super) async fn seed_store(pool: &PgPool, label: &str) -> (Uuid, Uuid) {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed user");

    let store_id = Uuid::new_v4();
    sqlx::query("INSERT INTO stores (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(store_id)
        .bind(format!("store-{label}"))
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed store");

    (user_id, store_id)
}

/// Seed a store owned by an existing user.
async fn seed_store_for(pool: &PgPool, user_id: Uuid, label: &str) -> Uuid {
    let store_id = Uuid::new_v4();
    sqlx::query("INSERT INTO stores (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(store_id)
        .bind(format!("store-{label}"))
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed store");
    store_id
}

/// Seed an old-shape payment method: its own xpub, its own counter.
async fn seed_method(
    pool: &PgPool,
    store_id: Uuid,
    chain_id: i64,
    token: Option<&str>,
    symbol: &str,
    xpub: &str,
    derivation_index: i32,
) -> Uuid {
    sqlx::query(
        "INSERT INTO store_payment_methods \
         (store_id, chain_id, token_address, asset_symbol, decimals, xpub, derivation_index) \
         VALUES ($1, $2, $3, $4, 18, $5, $6) RETURNING id",
    )
    .bind(store_id)
    .bind(chain_id)
    .bind(token)
    .bind(symbol)
    .bind(xpub)
    .bind(derivation_index)
    .fetch_one(pool)
    .await
    .expect("seed payment method")
    .get("id")
}

/// Seed an invoice with one payment option at a known address.
async fn seed_invoice_with_option(
    pool: &PgPool,
    store_id: Uuid,
    chain_id: i64,
    token: Option<&str>,
    symbol: &str,
    address: &str,
) -> (String, Uuid) {
    let invoice_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO invoices (id, store_id, currency, amount, expires_at) \
         VALUES ($1, $2, 'USD', 100, NOW() + interval '1 hour')",
    )
    .bind(&invoice_id)
    .bind(store_id)
    .execute(pool)
    .await
    .expect("seed invoice");

    let option_id: Uuid = sqlx::query(
        "INSERT INTO payment_options \
         (invoice_id, payment_method_id, chain_id, asset_type, asset_symbol, \
          token_address, decimals, payment_address, amount) \
         VALUES ($1, $2, $3, $4::asset_type, $5, $6, 18, $7, 1) RETURNING id",
    )
    .bind(&invoice_id)
    .bind(format!("{symbol}-{chain_id}"))
    .bind(chain_id)
    .bind(if token.is_some() { "erc20" } else { "native" })
    .bind(symbol)
    .bind(token)
    .bind(address)
    .fetch_one(pool)
    .await
    .expect("seed payment option")
    .get("id");

    (invoice_id, option_id)
}

// =========================================================================
// The migration
// =========================================================================

/// The load-bearing test: no address that has already been issued may be
/// re-issued, and no address already recorded may change.
///
/// The seed is the configuration that makes the old shape dangerous - one
/// store, two payment methods, one xpub, two counters at different positions -
/// plus a second store on the same key, which is the cross-store case the
/// migration has to handle.
#[tokio::test]
#[ignore]
async fn migration_never_re_derives_an_issued_address() {
    let (pool, name, server) = pre_migration_db("addresses").await;

    const SHARED: &str = "xpub-shared-key";
    let (user, store_a) = seed_store(&pool, "a").await;
    let store_b = seed_store_for(&pool, user, "b").await;

    // Store A: ETH at index 5, USDC at index 3, both on one xpub. Between them
    // they have issued indices 0..=4.
    let eth = seed_method(&pool, store_a, 1, None, "ETH", SHARED, 5).await;
    let usdc = seed_method(&pool, store_a, 1, Some("0xtoken"), "USDC", SHARED, 3).await;
    // Store B, same key, further along still.
    let b_eth = seed_method(&pool, store_b, 1, None, "ETH", SHARED, 9).await;

    // An address already handed to a customer on each of them.
    let (_, opt_eth) = seed_invoice_with_option(&pool, store_a, 1, None, "ETH", "0xaaa1").await;
    let (_, opt_usdc) =
        seed_invoice_with_option(&pool, store_a, 1, Some("0xtoken"), "USDC", "0xbbb2").await;
    let (_, opt_b) = seed_invoice_with_option(&pool, store_b, 1, None, "ETH", "0xccc3").await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    // One wallet for the key, not three.
    let wallets: i64 = sqlx::query("SELECT COUNT(*) AS c FROM wallets WHERE xpub = $1")
        .bind(SHARED)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("c");
    assert_eq!(
        wallets, 1,
        "one xpub must consolidate to one wallet, or it still has several counters"
    );

    // The counter is the MAX of what the merged rows had reached. Anything
    // lower re-issues an address that has already been given out.
    let index: i32 = sqlx::query("SELECT derivation_index FROM wallets WHERE xpub = $1")
        .bind(SHARED)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("derivation_index");
    assert_eq!(
        index, 9,
        "the merged counter must start above every index any of the merged \
         rows had issued (max was 9); {index} would re-derive"
    );

    // Every method now points at that one wallet.
    for (id, label) in [(eth, "eth"), (usdc, "usdc"), (b_eth, "store b eth")] {
        let w: Uuid = sqlx::query(
            "SELECT w.id FROM store_payment_methods pm \
             JOIN wallets w ON w.id = pm.wallet_id WHERE pm.id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("{label} lost its wallet: {e}"))
        .get("id");
        let xpub: String = sqlx::query("SELECT xpub FROM wallets WHERE id = $1")
            .bind(w)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("xpub");
        assert_eq!(xpub, SHARED, "{label} must still derive from the same key");
    }

    // And nothing already issued moved.
    for (opt, expected) in [(opt_eth, "0xaaa1"), (opt_usdc, "0xbbb2"), (opt_b, "0xccc3")] {
        let addr: String = sqlx::query("SELECT payment_address FROM payment_options WHERE id = $1")
            .bind(opt)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("payment_address");
        assert_eq!(
            addr, expected,
            "an address already given to a customer must survive the migration \
             byte for byte"
        );
    }

    drop_db(pool, &name, &server).await;
}

/// Routing must not move. A store that derived from key X before the migration
/// still derives from key X after it, even when the account's elected primary
/// is a different key.
#[tokio::test]
#[ignore]
async fn migration_preserves_which_key_each_store_uses() {
    let (pool, name, server) = pre_migration_db("routing").await;

    // One account, two stores, two different keys. The busy store's key wins
    // the primary election; the quiet one must not silently follow it.
    let (user, busy) = seed_store(&pool, "busy").await;
    let quiet = seed_store_for(&pool, user, "quiet").await;

    seed_method(&pool, busy, 1, None, "ETH", "xpub-busy", 2).await;
    seed_method(&pool, busy, 1, Some("0xt"), "USDC", "xpub-busy", 4).await;
    seed_method(&pool, quiet, 1, None, "ETH", "xpub-quiet", 1).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let primary: String = sqlx::query("SELECT xpub FROM wallets WHERE user_id = $1 AND is_primary")
        .bind(user)
        .fetch_one(&pool)
        .await
        .expect("an account with wallets must have exactly one primary")
        .get("xpub");
    assert_eq!(
        primary, "xpub-busy",
        "the key backing the most payment methods is the natural primary"
    );

    for (store, expected) in [(busy, "xpub-busy"), (quiet, "xpub-quiet")] {
        let xpub: String = sqlx::query(
            "SELECT w.xpub FROM store_wallets sw \
             JOIN wallets w ON w.id = sw.wallet_id WHERE sw.store_id = $1",
        )
        .bind(store)
        .fetch_one(&pool)
        .await
        .expect("every store with payment methods keeps an explicit override")
        .get("xpub");
        assert_eq!(
            xpub, expected,
            "leaving a store to fall through to the primary would move its \
             payouts to another key"
        );
    }

    drop_db(pool, &name, &server).await;
}

/// The partial unique index, not application code, is what holds "one primary".
#[tokio::test]
#[ignore]
async fn migration_leaves_exactly_one_primary_per_account() {
    let (pool, name, server) = pre_migration_db("primary").await;

    let (user, store) = seed_store(&pool, "p").await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-one", 0).await;
    seed_method(&pool, store, 137, None, "MATIC", "xpub-two", 0).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let primaries: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM wallets WHERE user_id = $1 AND is_primary")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
    assert_eq!(primaries, 1, "exactly one primary per account");

    // A second primary must be refused by the schema.
    let second =
        sqlx::query("UPDATE wallets SET is_primary = TRUE WHERE user_id = $1 AND NOT is_primary")
            .bind(user)
            .execute(&pool)
            .await;
    assert!(
        second.is_err(),
        "idx_account_wallets_one_primary must reject a second primary; if this \
         passes, the rule has quietly become application code again"
    );

    drop_db(pool, &name, &server).await;
}

/// The down migration refuses when the merge it would have to undo is
/// ambiguous, and works when it is not.
#[tokio::test]
#[ignore]
async fn down_migration_refuses_to_split_a_merged_counter() {
    let (pool, name, server) = pre_migration_db("down").await;

    let (_, store) = seed_store(&pool, "d").await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-merged", 5).await;
    seed_method(&pool, store, 1, Some("0xt"), "USDC", "xpub-merged", 3).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let down = std::fs::read_to_string(migrations_dir().join(format!("{MIGRATION}.down.sql")))
        .expect("read down migration");
    let err = pool
        .execute(down.as_str())
        .await
        .expect_err("a down that silently split the counter would re-derive addresses");
    assert!(
        err.to_string().contains("cannot be split back"),
        "the refusal must say why, not just fail: {err}"
    );

    drop_db(pool, &name, &server).await;
}

/// ... and reverses cleanly when nothing was merged, which is the case a
/// rollback of a bad deploy actually hits.
#[tokio::test]
#[ignore]
async fn down_migration_reverses_when_no_wallet_is_shared() {
    let (pool, name, server) = pre_migration_db("downok").await;

    let (_, store) = seed_store(&pool, "d2").await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-solo-a", 5).await;
    seed_method(&pool, store, 137, None, "MATIC", "xpub-solo-b", 2).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let down = std::fs::read_to_string(migrations_dir().join(format!("{MIGRATION}.down.sql")))
        .expect("read down migration");
    pool.execute(down.as_str())
        .await
        .expect("down must reverse");

    let rows = sqlx::query(
        "SELECT xpub, derivation_index FROM store_payment_methods \
         WHERE store_id = $1 ORDER BY xpub",
    )
    .bind(store)
    .fetch_all(&pool)
    .await
    .expect("old columns are back");

    let restored: Vec<(String, i32)> = rows
        .iter()
        .map(|r| (r.get("xpub"), r.get("derivation_index")))
        .collect();
    assert_eq!(
        restored,
        vec![
            ("xpub-solo-a".to_string(), 5),
            ("xpub-solo-b".to_string(), 2)
        ],
        "an unmerged counter must come back exactly where it was"
    );

    drop_db(pool, &name, &server).await;
}

/// One xpub spread across two accounts must not produce two counters that
/// overlap, and must not produce two that collide on their very first
/// allocation either.
///
/// Ownership of a shared key cannot be arbitrated, so each account keeps its
/// own wallet row. If each of those started at its own owner's high-water mark,
/// the lower one would issue straight through the range the other has already
/// spent - collisions the migration itself created, on top of any it inherited.
/// Starting both at the shared mark fixes that and introduces a worse one: the
/// same first index for both. They are staggered by a 100k stride instead.
#[tokio::test]
#[ignore]
async fn migration_does_not_create_new_collisions_across_accounts() {
    let (pool, name, server) = pre_migration_db("crossaccount").await;

    const SHARED: &str = "xpub-two-owners";
    let (_owner_a, store_a) = seed_store(&pool, "a").await;
    let (_owner_b, store_b) = seed_store(&pool, "b").await;

    // Owner A is far along on the key; owner B has barely used it.
    seed_method(&pool, store_a, 1, None, "ETH", SHARED, 9).await;
    seed_method(&pool, store_b, 1, None, "ETH", SHARED, 3).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let indices: Vec<i32> = sqlx::query(
        "SELECT derivation_index FROM wallets WHERE xpub = $1 ORDER BY derivation_index",
    )
    .bind(SHARED)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get("derivation_index"))
    .collect();

    assert_eq!(
        indices.len(),
        2,
        "a key shared by two accounts stays two wallets - there is no correct \
         owner to award it to"
    );
    assert_eq!(
        indices,
        vec![9, 100_009],
        "both wallets must start at or above the global high-water mark for the \
         key - leaving owner B at 3 makes it issue 3..9, every one of which \
         owner A has already given to a customer - and they must not start at \
         the SAME mark either, or their first allocations are one address"
    );

    drop_db(pool, &name, &server).await;
}

/// The down migration refuses when a counter has nowhere to land.
///
/// A wallet at a non-zero index whose payment method was deleted passes a guard
/// that only looks at sharing. Dropping it loses the counter, and re-adding the
/// xpub starts at 0 and re-issues everything it already produced.
#[tokio::test]
#[ignore]
async fn down_migration_refuses_to_strand_a_counter() {
    let (pool, name, server) = pre_migration_db("downstranded").await;

    let (_, store) = seed_store(&pool, "s").await;
    let method = seed_method(&pool, store, 1, None, "ETH", "xpub-stranded", 50).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    // The method goes away; the wallet and its counter remain.
    sqlx::query("DELETE FROM store_wallets WHERE store_id = $1")
        .bind(store)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM store_payment_methods WHERE id = $1")
        .bind(method)
        .execute(&pool)
        .await
        .unwrap();

    let down = std::fs::read_to_string(migrations_dir().join(format!("{MIGRATION}.down.sql")))
        .expect("read down migration");
    let err = pool
        .execute(down.as_str())
        .await
        .expect_err("a counter with nowhere to land must stop the reversal");
    assert!(
        err.to_string().contains("no payment method to carry"),
        "the refusal must name the reason: {err}"
    );

    drop_db(pool, &name, &server).await;
}

/// Duplicate native-asset methods are collapsed, and their rotation history
/// survives on the row that remains.
#[tokio::test]
#[ignore]
async fn migration_collapses_duplicate_native_methods_keeping_audit() {
    let (pool, name, server) = pre_migration_db("native").await;

    let (_, store) = seed_store(&pool, "n").await;
    // Two ETH-on-mainnet rows: impossible to prevent in the old shape, because
    // token_address is NULL and the composite unique index cannot see them.
    let older = seed_method(&pool, store, 1, None, "ETH", "xpub-native-a", 4).await;
    let newer = seed_method(&pool, store, 1, None, "ETH", "xpub-native-b", 2).await;

    sqlx::query(
        "INSERT INTO wallet_rotations \
         (store_id, previous_xpub, new_xpub, payment_method_id, previous_derivation_index) \
         VALUES ($1, 'old', 'new', $2, 1)",
    )
    .bind(store)
    .bind(newer)
    .execute(&pool)
    .await
    .expect("seed rotation history on the row that will be collapsed");

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let surviving: Vec<uuid::Uuid> = sqlx::query(
        "SELECT id FROM store_payment_methods WHERE store_id = $1 AND token_address IS NULL",
    )
    .bind(store)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get("id"))
    .collect();
    assert_eq!(surviving, vec![older], "the oldest row survives");

    let audit: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM wallet_rotations WHERE payment_method_id = $1")
            .bind(older)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
    assert_eq!(
        audit, 1,
        "rotation history must be repointed onto the survivor - the FK is ON \
         DELETE CASCADE, so deleting the duplicate outright destroys the audit \
         trail of a key that was rotated for a reason"
    );

    drop_db(pool, &name, &server).await;
}

/// A key that was rotated away from still counts towards its high-water mark.
///
/// The old rotation wrote `xpub = $new, derivation_index = 0` onto the method,
/// so a method rotated X -> Y -> X holds X at index 0 while having already
/// issued from it. `store_payment_methods` alone therefore under-reports the
/// key, and a wallet started from that number re-issues addresses X has
/// already produced. `wallet_rotations.previous_derivation_index` is the only
/// surviving record of how far it got.
#[tokio::test]
#[ignore]
async fn migration_counts_a_rotated_away_key_towards_its_high_water_mark() {
    let (pool, name, server) = pre_migration_db("rotatedaway").await;

    const KEY: &str = "xpub-came-back";
    let (_, store) = seed_store(&pool, "r").await;

    // Where the method landed after being rotated back onto KEY: index 0.
    let method = seed_method(&pool, store, 1, None, "ETH", KEY, 0).await;

    // What KEY had issued before it was rotated away.
    sqlx::query(
        "INSERT INTO wallet_rotations \
         (store_id, previous_xpub, new_xpub, payment_method_id, previous_derivation_index) \
         VALUES ($1, $2, 'xpub-interlude', $3, 20)",
    )
    .bind(store)
    .bind(KEY)
    .bind(method)
    .execute(&pool)
    .await
    .expect("seed the rotation that took KEY out of service");

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let index: i32 = sqlx::query("SELECT derivation_index FROM wallets WHERE xpub = $1")
        .bind(KEY)
        .fetch_one(&pool)
        .await
        .expect("the key has a wallet")
        .get("derivation_index");

    assert_eq!(
        index, 20,
        "the wallet must start above every index this key has issued, including \
         the range it issued before being rotated away. Starting at the \
         method's 0 re-derives 0..19 - addresses customers already hold"
    );

    drop_db(pool, &name, &server).await;
}

/// Duplicate native methods are collapsed BEFORE the primary is elected and
/// the store override is written, so neither can name a wallet that ends up
/// backing nothing.
///
/// Ordering is the whole test. Counting methods first elects W2 (two rows to
/// W1's one), then deletes both of W2's rows as duplicates - leaving the
/// account primary and the store override pointing at a wallet no surviving
/// method derives from, while the one method left collects on W1.
#[tokio::test]
#[ignore]
async fn migration_elects_a_primary_that_survives_duplicate_collapse() {
    let (pool, name, server) = pre_migration_db("electorder").await;

    let (owner, store) = seed_store(&pool, "e").await;

    // The oldest ETH row, and the one that survives the collapse.
    seed_method(&pool, store, 1, None, "ETH", "xpub-survivor", 4).await;
    // Two more ETH-on-mainnet rows on a second key: a majority by count, and
    // duplicates that are about to be deleted.
    seed_method(&pool, store, 1, None, "ETH", "xpub-doomed", 2).await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-doomed", 3).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let primary_xpub: String =
        sqlx::query("SELECT xpub FROM wallets WHERE user_id = $1 AND is_primary")
            .bind(owner)
            .fetch_one(&pool)
            .await
            .expect("exactly one primary")
            .get("xpub");
    assert_eq!(
        primary_xpub, "xpub-survivor",
        "the primary must be elected from the methods that survive, not from \
         duplicates the same migration is about to delete"
    );

    let override_xpub: String = sqlx::query(
        "SELECT w.xpub FROM store_wallets sw JOIN wallets w ON w.id = sw.wallet_id \
         WHERE sw.store_id = $1",
    )
    .bind(store)
    .fetch_one(&pool)
    .await
    .expect("the store keeps an explicit override")
    .get("xpub");

    let derived_xpub: String = sqlx::query(
        "SELECT w.xpub FROM store_payment_methods pm JOIN wallets w ON w.id = pm.wallet_id \
         WHERE pm.store_id = $1",
    )
    .bind(store)
    .fetch_one(&pool)
    .await
    .expect("one method survives")
    .get("xpub");

    assert_eq!(
        override_xpub, derived_xpub,
        "what the store reports and what it derives from have to be the same \
         key - that equality is the promise the wallet endpoints make"
    );

    drop_db(pool, &name, &server).await;
}

/// Provenance is left NULL where the join key was never unique.
///
/// A native option is matched back to a method through (store, chain, token),
/// and token is NULL for native assets - so while duplicates existed that key
/// named several methods, each formerly with its own xpub. The survivor's
/// wallet is a plausible answer, not a known one, and the migration's own rule
/// is that a wrong stamp is worse than an honest NULL.
#[tokio::test]
#[ignore]
async fn migration_leaves_provenance_null_where_the_method_was_never_unique() {
    let (pool, name, server) = pre_migration_db("ambiguous").await;

    let (_, store) = seed_store(&pool, "p").await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-amb-a", 4).await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-amb-b", 2).await;
    // A token method on the same store, where the key IS unique.
    seed_method(&pool, store, 1, Some("0xtok"), "USDC", "xpub-amb-c", 1).await;

    let (_, native_option) =
        seed_invoice_with_option(&pool, store, 1, None, "ETH", "0xnative").await;
    let (_, token_option) =
        seed_invoice_with_option(&pool, store, 1, Some("0xtok"), "USDC", "0xtoken").await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    let native_wallet: Option<Uuid> =
        sqlx::query("SELECT wallet_id FROM payment_options WHERE id = $1")
            .bind(native_option)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("wallet_id");
    assert!(
        native_wallet.is_none(),
        "the native option could have been served by either duplicate, so its \
         provenance is unknown and must stay NULL rather than name the row \
         that happened to survive"
    );

    let token_wallet: Option<Uuid> =
        sqlx::query("SELECT wallet_id FROM payment_options WHERE id = $1")
            .bind(token_option)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("wallet_id");
    assert!(
        token_wallet.is_some(),
        "the token option's method was unique all along - skipping it too would \
         throw away provenance that is actually known"
    );

    drop_db(pool, &name, &server).await;
}

/// The down migration refuses a wallet shared by methods that INHERIT it.
///
/// `store_payment_methods.wallet_id` is NULL on every inheriting method, so a
/// guard that reads only that column sees no sharing at all and lets the
/// reversal copy one index onto all of them - the exact merge it exists to
/// refuse. Using the per-store override is what produces this state:
/// `set_store_wallet` NULLs every method in the store.
#[tokio::test]
#[ignore]
async fn down_migration_refuses_a_wallet_shared_by_inheriting_methods() {
    let (pool, name, server) = pre_migration_db("downinherit").await;

    let (_, store) = seed_store(&pool, "di").await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-inherit", 5).await;
    seed_method(&pool, store, 137, None, "MATIC", "xpub-inherit-2", 2).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    // What `PUT /stores/{id}/wallet` leaves behind: one override, no pins.
    sqlx::query("UPDATE store_payment_methods SET wallet_id = NULL WHERE store_id = $1")
        .bind(store)
        .execute(&pool)
        .await
        .expect("unpin every method, as setting an override does");

    let down = std::fs::read_to_string(migrations_dir().join(format!("{MIGRATION}.down.sql")))
        .expect("read down migration");
    let err = pool
        .execute(down.as_str())
        .await
        .expect_err("two inheriting methods on one wallet is still a merge");
    assert!(
        err.to_string().contains("cannot be split back"),
        "the refusal must name the merge, not fail on a NOT NULL later: {err}"
    );

    drop_db(pool, &name, &server).await;
}

/// ... and does NOT refuse a wallet reached only through a store override.
///
/// A wallet with no pinned method still has somewhere to land its counter if a
/// store resolves to it. Refusing that case blocks a rollback the migration can
/// in fact perform losslessly.
#[tokio::test]
#[ignore]
async fn down_migration_reverses_a_wallet_reached_only_through_an_override() {
    let (pool, name, server) = pre_migration_db("downoverride").await;

    let (_, store) = seed_store(&pool, "do").await;
    seed_method(&pool, store, 1, None, "ETH", "xpub-only-override", 7).await;

    pool.execute(migration_sql(MIGRATION).as_str())
        .await
        .expect("apply the migration under test");

    sqlx::query("UPDATE store_payment_methods SET wallet_id = NULL WHERE store_id = $1")
        .bind(store)
        .execute(&pool)
        .await
        .expect("unpin, leaving the override as the only route to the wallet");

    let down = std::fs::read_to_string(migrations_dir().join(format!("{MIGRATION}.down.sql")))
        .expect("read down migration");
    pool.execute(down.as_str())
        .await
        .expect("a counter with an override to land on is not stranded");

    let restored: (String, i32) =
        sqlx::query("SELECT xpub, derivation_index FROM store_payment_methods WHERE store_id = $1")
            .bind(store)
            .fetch_one(&pool)
            .await
            .map(|r| (r.get("xpub"), r.get("derivation_index")))
            .expect("old columns are back");

    assert_eq!(
        restored,
        ("xpub-only-override".to_string(), 7),
        "the inherited key and its counter come back onto the method that was \
         in fact using them"
    );

    drop_db(pool, &name, &server).await;
}
