//! Account wallet repository implementation.

use async_trait::async_trait;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::PgDataService;
use crate::{
    RepositoryError, RepositoryResult, Wallet, WalletReader, WalletWriter, sqlx_to_repo_error,
};

/// Every column of `wallets`, in the order `row_to_wallet` reads them.
const WALLET_COLUMNS: &str =
    "id, user_id, namespace, xpub, derivation_index, name, is_primary, created_at";

/// The same list, aliased. `stores` also has an `id`, so an unqualified list
/// in a query that joins the two is ambiguous and Postgres rejects it.
const WALLET_COLUMNS_W: &str = "w.id, w.user_id, w.namespace, w.xpub, w.derivation_index, \
     w.name, w.is_primary, w.created_at";

/// The chain family of a payment method aliased `pm`, as SQL.
///
/// A method's family is its chain id's CAIP-2 namespace, and the schema stores
/// it as a generated column so the two cannot disagree. Referenced by name
/// rather than recomputed, because a query that recomputed it would be free to
/// compute it differently.
pub(super) const METHOD_NAMESPACE: &str = "pm.chain_namespace";

/// Resolve the wallet a store derives from *for one chain family*: its
/// override for that family, else the owner's primary for that family. `$1` is
/// the store id; `namespace` is a SQL expression naming the family, either a
/// bind parameter or a column.
///
/// Spelled once and reused, because the same chain has to be walked by reads,
/// by allocation and by rotation. A second spelling that drifts is a store
/// quietly collecting on a key the UI never showed.
///
/// The namespace filter is the point rather than a refinement. An account-level
/// xpub has its BIP-44 coin type baked in, so answering a `tron:` method with
/// an `eip155` wallet does not produce a slightly-wrong address, it produces a
/// valid `T...` address at coin type 60 that the merchant's wallet never
/// watches. Resolving to nothing, and refusing the invoice, is the only honest
/// answer this walk can give when the family has no key.
pub(super) fn store_wallet_resolution(namespace: &str) -> String {
    format!(
        "COALESCE(
        (SELECT sw.wallet_id FROM store_wallets sw
          WHERE sw.store_id = s.id AND sw.namespace = {namespace}),
        (SELECT p.id FROM wallets p
          WHERE p.user_id = s.owner_id AND p.is_primary AND p.namespace = {namespace})
    )"
    )
}

impl PgDataService {
    /// Whether this store resolves to a wallet for `namespace` without a
    /// method-level pin.
    ///
    /// The same walk every read uses - the store's own wallet for that family,
    /// else the account primary for it. Asked before creating an unpinned
    /// payment method, so a merchant finds out at setup rather than when a
    /// customer is waiting to pay - and asked per family, because an account
    /// with an Ethereum key and no Tron one resolves for one and not the
    /// other.
    pub(super) async fn store_resolves_to_a_wallet(
        &self,
        store_id: Uuid,
        namespace: &str,
    ) -> RepositoryResult<bool> {
        let resolved: Option<Uuid> = sqlx::query_scalar(&format!(
            "SELECT {} FROM stores s WHERE s.id = $1",
            store_wallet_resolution("$2")
        ))
        .bind(store_id)
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?
        .flatten();
        Ok(resolved.is_some())
    }
}

fn row_to_wallet(row: &sqlx::postgres::PgRow) -> Wallet {
    Wallet {
        id: row.get("id"),
        user_id: row.get("user_id"),
        namespace: row.get("namespace"),
        xpub: row.get("xpub"),
        derivation_index: row.get("derivation_index"),
        name: row.get("name"),
        is_primary: row.get("is_primary"),
        created_at: row.get("created_at"),
    }
}

/// Serialise everything that touches one account's primary flag.
///
/// `is_primary` is decided with `NOT EXISTS (... WHERE user_id = $1)` while the
/// insert's `ON CONFLICT` targets `(user_id, namespace, xpub)` - a different index from
/// the partial unique one that enforces the primary. Two concurrent creates on
/// a fresh account therefore both evaluate that to true, and one loses on an
/// index it was not conflicting against, surfacing as a bare unique violation.
/// That is reachable from the ordinary "enable ETH, enable USDC" flow, so it is
/// locked rather than retried.
pub(super) async fn lock_account(conn: &mut PgConnection, user_id: Uuid) -> RepositoryResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('wallet:user'), hashtext($1::text))")
        .bind(user_id)
        .execute(conn)
        .await
        .map_err(sqlx_to_repo_error)?;
    Ok(())
}

/// Serialise everything that registers a given xpub, across all accounts.
///
/// There is no global unique index on `xpub` - the migration cannot create one,
/// because a key already shared by two accounts has no correct owner to award
/// it to - so cross-account exclusivity is checked rather than constrained. A
/// check alone races: two accounts registering the same key at once would both
/// find it free. This closes that window.
///
/// Always taken AFTER `lock_account`, so the two never deadlock against each
/// other.
pub(super) async fn lock_xpub(conn: &mut PgConnection, xpub: &str) -> RepositoryResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('wallet:xpub'), hashtext($1))")
        .bind(xpub)
        .execute(conn)
        .await
        .map_err(sqlx_to_repo_error)?;
    Ok(())
}

/// Take the account lock for whoever owns `store_id`.
///
/// Everything that changes where an account's money is collected takes this
/// lock first, so those writers are ordered against each other rather than
/// against whichever row they happen to touch first. Rotation writes payment
/// methods then the override; setting an override writes the override then the
/// payment methods. Without a lock in front, those two orders deadlock - which
/// Postgres does detect and abort, but as a 500 on a request that had nothing
/// wrong with it.
async fn lock_store_account(conn: &mut PgConnection, store_id: Uuid) -> RepositoryResult<()> {
    let owner = sqlx::query("SELECT owner_id FROM stores WHERE id = $1")
        .bind(store_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sqlx_to_repo_error)?
        .ok_or_else(|| RepositoryError::NotFound("store not found".into()))?;

    lock_account(conn, owner.get("owner_id")).await
}

/// Refuse an xpub that another account already holds.
///
/// Two accounts deriving from one key is the same collision as two counters on
/// one key, reached from the other direction: each counts independently and
/// both issue index 0, 1, 2 to different merchants' customers. Within an
/// account `idx_account_wallets_user_xpub` makes it impossible; across accounts
/// this is the enforcement, and it only holds because the caller is holding
/// `lock_xpub`.
pub(super) async fn reject_if_another_account_holds(
    conn: &mut PgConnection,
    user_id: Uuid,
    xpub: &str,
) -> RepositoryResult<()> {
    let taken = sqlx::query("SELECT 1 AS x FROM wallets WHERE xpub = $1 AND user_id <> $2 LIMIT 1")
        .bind(xpub)
        .bind(user_id)
        .fetch_optional(conn)
        .await
        .map_err(sqlx_to_repo_error)?;

    if taken.is_some() {
        return Err(RepositoryError::Conflict(
            "this xpub is already registered to another account".into(),
        ));
    }
    Ok(())
}

/// Find or create the wallet holding `xpub` for `user_id` in `namespace`,
/// inside a caller's transaction that already holds both locks.
///
/// The conflict target is `(user_id, namespace, xpub)`. The same bytes in two
/// families are two wallets with two counters, which is safe here in a way it
/// is not within one family: the addresses those counters produce live on
/// different chains, so no customer can be handed an address another customer
/// already has. Keyed on `(user_id, xpub)` this would hand a merchant asking
/// to register a Tron key their existing Ethereum wallet, and every Tron
/// address they then quoted would be derived at coin type 60.
pub(super) async fn upsert_wallet(
    conn: &mut PgConnection,
    user_id: Uuid,
    namespace: &str,
    xpub: &str,
    name: Option<&str>,
) -> RepositoryResult<Wallet> {
    // `DO UPDATE` rather than `DO NOTHING`: the latter returns no row on
    // conflict and the caller would see a spurious "not found" for a wallet
    // that plainly exists.
    //
    // `is_primary` is decided per namespace, so the first Tron key an account
    // registers becomes its Tron primary even though it already has an
    // Ethereum one. Deciding it account-wide would leave every Tron store
    // resolving to nothing until the merchant found a "make primary" button
    // they have no reason to look for.
    let row = sqlx::query(&format!(
        r#"
        INSERT INTO wallets (user_id, namespace, xpub, name, is_primary)
        VALUES ($1, $2, $3, $4,
                NOT EXISTS (SELECT 1 FROM wallets WHERE user_id = $1 AND namespace = $2))
        ON CONFLICT (user_id, namespace, xpub) DO UPDATE
            SET name = COALESCE(EXCLUDED.name, wallets.name)
        RETURNING {WALLET_COLUMNS}
        "#
    ))
    .bind(user_id)
    .bind(namespace)
    .bind(xpub)
    .bind(name)
    .fetch_one(conn)
    .await
    .map_err(sqlx_to_repo_error)?;

    Ok(row_to_wallet(&row))
}

impl PgDataService {
    /// Find or create the account wallet holding `xpub` for a store's owner,
    /// in the chain family the key is being configured for.
    ///
    /// Payment methods are still configured by pasting an xpub, so this is
    /// where that xpub becomes a wallet - and where the same cross-account
    /// refusal applies, since configuring a method is another way in.
    ///
    /// `namespace` comes from the method's own chain, never from a default:
    /// pasting a key onto a `tron:` method is a statement that the key is a
    /// Tron key, and filing it as Ethereum here would be the whole bug reached
    /// through the payment-method form instead of the wallet one.
    pub(super) async fn wallet_for_store_xpub(
        &self,
        store_id: Uuid,
        namespace: &str,
        xpub: &str,
    ) -> RepositoryResult<Uuid> {
        let mut tx = self.pool.begin().await.map_err(sqlx_to_repo_error)?;

        let owner = sqlx::query("SELECT owner_id FROM stores WHERE id = $1")
            .bind(store_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_to_repo_error)?
            .ok_or_else(|| RepositoryError::NotFound("store not found".into()))?;
        let owner_id: Uuid = owner.get("owner_id");

        lock_account(&mut tx, owner_id).await?;
        lock_xpub(&mut tx, xpub).await?;
        reject_if_another_account_holds(&mut tx, owner_id, xpub).await?;

        let wallet = upsert_wallet(&mut tx, owner_id, namespace, xpub, None).await?;
        tx.commit().await.map_err(sqlx_to_repo_error)?;
        Ok(wallet.id)
    }
}

#[async_trait]
impl WalletReader for PgDataService {
    async fn get_wallet(&self, wallet_id: Uuid) -> RepositoryResult<Option<Wallet>> {
        let row = sqlx::query(&format!(
            "SELECT {WALLET_COLUMNS} FROM wallets WHERE id = $1"
        ))
        .bind(wallet_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.as_ref().map(row_to_wallet))
    }

    async fn list_wallets(&self, user_id: Uuid) -> RepositoryResult<Vec<Wallet>> {
        let rows = sqlx::query(&format!(
            "SELECT {WALLET_COLUMNS} FROM wallets WHERE user_id = $1 \
             ORDER BY is_primary DESC, created_at ASC, id ASC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows.iter().map(row_to_wallet).collect())
    }

    async fn get_primary_wallet(
        &self,
        user_id: Uuid,
        namespace: &str,
    ) -> RepositoryResult<Option<Wallet>> {
        let row = sqlx::query(&format!(
            "SELECT {WALLET_COLUMNS} FROM wallets \
             WHERE user_id = $1 AND namespace = $2 AND is_primary"
        ))
        .bind(user_id)
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.as_ref().map(row_to_wallet))
    }

    async fn resolve_store_wallet(
        &self,
        store_id: Uuid,
        namespace: &str,
    ) -> RepositoryResult<Option<Wallet>> {
        // Override first, primary second, in one round trip. Two queries would
        // let a `set_primary_wallet` land between them and resolve a store to
        // a wallet that was never either of its answers.
        let row = sqlx::query(&format!(
            r#"
            SELECT {WALLET_COLUMNS_W}
            FROM wallets w
            JOIN stores s ON s.id = $1
            WHERE w.id = {}
            "#,
            store_wallet_resolution("$2")
        ))
        .bind(store_id)
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.as_ref().map(row_to_wallet))
    }

    async fn get_store_wallet_override(
        &self,
        store_id: Uuid,
        namespace: &str,
    ) -> RepositoryResult<Option<Uuid>> {
        let row = sqlx::query(
            "SELECT wallet_id FROM store_wallets WHERE store_id = $1 AND namespace = $2",
        )
        .bind(store_id)
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.map(|r| r.get("wallet_id")))
    }
}

#[async_trait]
impl WalletWriter for PgDataService {
    async fn create_wallet(
        &self,
        user_id: Uuid,
        namespace: &str,
        xpub: &str,
        name: Option<&str>,
    ) -> RepositoryResult<Wallet> {
        let mut tx = self.pool.begin().await.map_err(sqlx_to_repo_error)?;

        lock_account(&mut tx, user_id).await?;
        lock_xpub(&mut tx, xpub).await?;
        reject_if_another_account_holds(&mut tx, user_id, xpub).await?;

        // Re-adding an xpub the account already holds IN THIS FAMILY returns
        // the existing row. A second row would be a second counter on one key,
        // which is the collision this module exists to prevent. The first
        // wallet an account registers in a family becomes that family's
        // primary, so a merchant with one wallet per chain never has to meet
        // the concept.
        let wallet = upsert_wallet(&mut tx, user_id, namespace, xpub, name).await?;

        tx.commit().await.map_err(sqlx_to_repo_error)?;
        Ok(wallet)
    }

    async fn rename_wallet(&self, wallet_id: Uuid, name: Option<&str>) -> RepositoryResult<Wallet> {
        let row = sqlx::query(&format!(
            "UPDATE wallets SET name = $2 WHERE id = $1 RETURNING {WALLET_COLUMNS}"
        ))
        .bind(wallet_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        row.as_ref()
            .map(row_to_wallet)
            .ok_or_else(|| RepositoryError::NotFound("wallet not found".into()))
    }

    async fn set_primary_wallet(&self, user_id: Uuid, wallet_id: Uuid) -> RepositoryResult<Wallet> {
        let mut tx = self.pool.begin().await.map_err(sqlx_to_repo_error)?;
        lock_account(&mut tx, user_id).await?;

        // Demote before promoting. `idx_account_wallets_one_primary` is an
        // immediate unique index, so promoting first would fail against the
        // outgoing primary even inside a transaction.
        //
        // Scoped to the named wallet's own family, read from its row rather
        // than taken from the caller. Promoting a Tron wallet must demote the
        // Tron primary and nothing else: demoting account-wide would leave
        // every EVM store on the account with no primary to fall back to, and
        // the merchant would discover it the next time a customer tried to pay
        // in ETH. A wallet id that is not this user's matches no row, so the
        // subquery is NULL, the demotion touches nothing, and the promotion
        // below reports not-found - which is the tenancy answer as well.
        sqlx::query(
            "UPDATE wallets SET is_primary = FALSE \
             WHERE user_id = $1 AND is_primary \
               AND namespace = (SELECT namespace FROM wallets WHERE id = $2 AND user_id = $1)",
        )
        .bind(user_id)
        .bind(wallet_id)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_to_repo_error)?;

        // `user_id = $1` in the promotion is the tenancy check: without it a
        // caller could hand over someone else's wallet id and take it over.
        let row = sqlx::query(&format!(
            "UPDATE wallets SET is_primary = TRUE WHERE id = $2 AND user_id = $1 \
             RETURNING {WALLET_COLUMNS}"
        ))
        .bind(user_id)
        .bind(wallet_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_to_repo_error)?;

        let wallet = row
            .as_ref()
            .map(row_to_wallet)
            .ok_or_else(|| RepositoryError::NotFound("wallet not found".into()))?;

        tx.commit().await.map_err(sqlx_to_repo_error)?;
        Ok(wallet)
    }

    async fn delete_wallet(&self, wallet_id: Uuid) -> RepositoryResult<()> {
        // `store_payment_methods.wallet_id` and `store_wallets.wallet_id` are
        // ON DELETE RESTRICT, so a wallet in use raises a foreign key
        // violation here. `payment_options.wallet_id` is ON DELETE SET NULL by
        // contrast: history must never be the reason a wallet cannot be
        // removed. Translate the violation rather than let it surface as a
        // 500 - "in use" is something the caller can act on.
        let result = sqlx::query("DELETE FROM wallets WHERE id = $1")
            .bind(wallet_id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if let sqlx::Error::Database(ref db) = e
                    && db.is_foreign_key_violation()
                {
                    return RepositoryError::Conflict(
                        "wallet is still used by a store or payment method".into(),
                    );
                }
                sqlx_to_repo_error(e)
            })?;

        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound("wallet not found".into()));
        }

        Ok(())
    }

    async fn set_store_wallet(&self, store_id: Uuid, wallet_id: Uuid) -> RepositoryResult<()> {
        let mut tx = self.pool.begin().await.map_err(sqlx_to_repo_error)?;
        lock_store_account(&mut tx, store_id).await?;

        // The wallet has to belong to the store's owner. Enforced in the
        // predicate rather than by a prior SELECT so there is no window in
        // which ownership changes between check and write; zero rows inserted
        // means the pairing was not legitimate.
        //
        // The override's family comes from the wallet, so the two cannot
        // disagree - and the composite foreign key in the schema refuses the
        // row if a later edit ever made them.
        let result = sqlx::query(
            r#"
            INSERT INTO store_wallets (store_id, wallet_id, namespace)
            SELECT s.id, w.id, w.namespace
            FROM stores s
            JOIN wallets w ON w.user_id = s.owner_id
            WHERE s.id = $1 AND w.id = $2
            ON CONFLICT (store_id, namespace) DO UPDATE SET wallet_id = EXCLUDED.wallet_id
            "#,
        )
        .bind(store_id)
        .bind(wallet_id)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_to_repo_error)?;

        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(
                "store or wallet not found, or the wallet belongs to another account".into(),
            ));
        }

        // Release the store's payment methods so the override actually decides
        // where money goes.
        //
        // Without this the endpoint is cosmetic: the override row changes, the
        // GET reports the new wallet, and the next invoice still derives from
        // whatever each method was pinned to. Every method the migration
        // touched is pinned - that is what kept routing identical on day one -
        // so "give this store its own wallet" has to mean the store's methods
        // now follow the store. They stay unpinned afterwards, so a later
        // change of primary reaches them too.
        //
        // Only the methods in this wallet's family, though. Unpinning them all
        // would make "pin this store to my Tron wallet" silently move the
        // store's ETH and USDC collection onto the account's Ethereum primary,
        // which is a change of where real money goes ordered by a request that
        // said nothing about Ethereum.
        sqlx::query(
            "UPDATE store_payment_methods SET wallet_id = NULL \
             WHERE store_id = $1 \
               AND chain_namespace = (SELECT namespace FROM wallets WHERE id = $2)",
        )
        .bind(store_id)
        .bind(wallet_id)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_to_repo_error)?;

        tx.commit().await.map_err(sqlx_to_repo_error)?;
        Ok(())
    }

    async fn clear_store_wallet(&self, store_id: Uuid, namespace: &str) -> RepositoryResult<()> {
        let mut tx = self.pool.begin().await.map_err(sqlx_to_repo_error)?;
        lock_store_account(&mut tx, store_id).await?;

        // Idempotent: no override is the state the caller asked for, so
        // clearing twice is not an error.
        //
        // Scoped to one family, mirroring `set_store_wallet`. A store may hold
        // an override per family, and "stop overriding for Tron" must not also
        // stop overriding for Ethereum - the caller named one chain.
        sqlx::query("DELETE FROM store_wallets WHERE store_id = $1 AND namespace = $2")
            .bind(store_id)
            .bind(namespace)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_repo_error)?;

        // Unpin too, for the same reason `set_store_wallet` does: otherwise
        // "stop overriding" would leave methods pinned to the wallet the
        // override used to name, and the store would keep deriving from it
        // while reporting the primary.
        sqlx::query(
            "UPDATE store_payment_methods SET wallet_id = NULL \
             WHERE store_id = $1 AND chain_namespace = $2",
        )
        .bind(store_id)
        .bind(namespace)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_to_repo_error)?;

        tx.commit().await.map_err(sqlx_to_repo_error)?;
        Ok(())
    }

    async fn next_derivation_index(&self, wallet_id: Uuid) -> RepositoryResult<i32> {
        // One statement, deliberately. `UPDATE ... RETURNING` takes a row lock
        // on the wallet for the duration, so concurrent allocations serialise
        // and each caller sees a distinct index; a SELECT followed by an
        // UPDATE would hand the same index to both under READ COMMITTED.
        //
        // The old code did the same thing on store_payment_methods, so
        // the statement was never the problem: the counter was. Two methods
        // sharing an xpub were two rows, each perfectly serialised against
        // itself and not at all against the other. Locking the wallet is what
        // makes them contend, which is the point.
        let row = sqlx::query(
            r#"
            UPDATE wallets
            SET derivation_index = derivation_index + 1
            WHERE id = $1
            RETURNING derivation_index - 1 AS current_index
            "#,
        )
        .bind(wallet_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        match row {
            Some(r) => Ok(r.get("current_index")),
            None => Err(RepositoryError::NotFound("wallet not found".into())),
        }
    }
}
