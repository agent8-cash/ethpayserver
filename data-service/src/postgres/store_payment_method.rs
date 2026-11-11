//! Store payment method repository implementation.

use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

use super::PgDataService;
use super::conversions::chain_id_from_row;
use super::wallet::{METHOD_NAMESPACE, store_wallet_resolution};
use crate::{RepositoryError, RepositoryResult, sqlx_to_repo_error};
use types::{DerivationAllocation, StorePaymentMethod};
use types::{StorePaymentMethodReader, StorePaymentMethodWriter};

/// The projection every read uses.
///
/// `wallet_id`, `xpub` and `derivation_index` are the *resolved* wallet's, not
/// the method's: the method holds at most a pin, and the key it
/// actually derives from is found by walking pin, store override, account
/// primary. Reading through that walk is what stops a caller rendering one key
/// while payments are collected on another.
const METHOD_COLUMNS: &str = "pm.id, pm.store_id, pm.chain_id, pm.token_address, \
     pm.asset_symbol, pm.decimals, w.id AS wallet_id, w.xpub, w.derivation_index, \
     pm.enabled, pm.created_at";

/// The wallet a payment method aliased `pm` derives from: its pin, else its
/// store's override for the method's family, else the account primary for that
/// family.
///
/// Every step is scoped to `pm.chain_namespace`. The pin is scoped by the
/// schema - a composite foreign key refuses a `wallet_id` from another family
/// outright - and the two fallbacks by the filter inside
/// [`store_wallet_resolution`]. What this must never do is answer a `tron:`
/// method with an `eip155` wallet: that key was exported at coin type 60 and
/// the `T...` address derived from it is one the merchant's wallet does not
/// watch.
fn method_wallet() -> String {
    format!(
        "COALESCE(pm.wallet_id, {})",
        store_wallet_resolution(METHOD_NAMESPACE)
    )
}

/// `FROM` clause resolving a payment method to the wallet it derives from.
///
/// LEFT JOIN, not JOIN: a method whose chain runs out - no pin, no store
/// override, no account primary for its family - must still be listed. It
/// exists and simply cannot be paid yet, which is a state the settings UI has
/// to be able to show. An inner join would silently hide it, and a merchant
/// would be left looking for a payment method they can see they created.
///
/// The `w.namespace` equality is belt and braces over the foreign key: if a
/// wallet from the wrong family ever did reach `pm.wallet_id`, this reports
/// the method as having no key rather than deriving from it.
fn method_from() -> String {
    format!(
        "FROM store_payment_methods pm \
         JOIN stores s ON s.id = pm.store_id \
         LEFT JOIN wallets w ON w.id = {} AND w.namespace = {METHOD_NAMESPACE}",
        method_wallet()
    )
}

fn row_to_payment_method(row: &sqlx::postgres::PgRow) -> StorePaymentMethod {
    let decimals: i16 = row.get("decimals");
    StorePaymentMethod {
        id: row.get("id"),
        store_id: row.get("store_id"),
        chain_id: chain_id_from_row(row, "chain_id"),
        token_address: row.get("token_address"),
        asset_symbol: row.get("asset_symbol"),
        decimals: decimals as u8,
        wallet_id: row.get("wallet_id"),
        xpub: row.get("xpub"),
        derivation_index: row.get("derivation_index"),
        enabled: row.get("enabled"),
        created_at: row.get("created_at"),
    }
}

#[async_trait]
impl StorePaymentMethodReader for PgDataService {
    async fn get_payment_methods(
        &self,
        store_id: Uuid,
    ) -> RepositoryResult<Vec<StorePaymentMethod>> {
        let rows = sqlx::query(&format!(
            "SELECT {METHOD_COLUMNS} {} WHERE pm.store_id = $1 ORDER BY pm.created_at",
            method_from()
        ))
        .bind(store_id)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows.iter().map(row_to_payment_method).collect())
    }

    async fn get_enabled_payment_methods(
        &self,
        store_id: Uuid,
    ) -> RepositoryResult<Vec<StorePaymentMethod>> {
        let rows = sqlx::query(&format!(
            "SELECT {METHOD_COLUMNS} {} \
             WHERE pm.store_id = $1 AND pm.enabled = true ORDER BY pm.created_at",
            method_from()
        ))
        .bind(store_id)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows.iter().map(row_to_payment_method).collect())
    }

    async fn get_payment_method(&self, id: Uuid) -> RepositoryResult<Option<StorePaymentMethod>> {
        let row = sqlx::query(&format!(
            "SELECT {METHOD_COLUMNS} {} WHERE pm.id = $1",
            method_from()
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(row.as_ref().map(row_to_payment_method))
    }

    async fn get_payment_method_by_chain(
        &self,
        store_id: Uuid,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
    ) -> RepositoryResult<Option<StorePaymentMethod>> {
        let row = match token_address {
            Some(addr) => {
                sqlx::query(&format!(
                    "SELECT {METHOD_COLUMNS} {} \
                     WHERE pm.store_id = $1 AND pm.chain_id = $2 AND pm.token_address = $3",
                    method_from()
                ))
                .bind(store_id)
                .bind(chain_id.as_str())
                .bind(addr)
                .fetch_optional(&self.pool)
                .await
            }
            None => {
                sqlx::query(&format!(
                    "SELECT {METHOD_COLUMNS} {} \
                     WHERE pm.store_id = $1 AND pm.chain_id = $2 AND pm.token_address IS NULL",
                    method_from()
                ))
                .bind(store_id)
                .bind(chain_id.as_str())
                .fetch_optional(&self.pool)
                .await
            }
        }
        .map_err(sqlx_to_repo_error)?;

        Ok(row.as_ref().map(row_to_payment_method))
    }

    async fn find_by_asset_symbol(
        &self,
        store_id: Uuid,
        asset_symbol: &str,
    ) -> RepositoryResult<Vec<StorePaymentMethod>> {
        let rows = sqlx::query(&format!(
            "SELECT {METHOD_COLUMNS} {} \
             WHERE pm.store_id = $1 AND pm.asset_symbol = $2 AND pm.enabled = true \
             ORDER BY pm.created_at",
            method_from()
        ))
        .bind(store_id)
        .bind(asset_symbol)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows.iter().map(row_to_payment_method).collect())
    }
}

#[async_trait]
impl StorePaymentMethodWriter for PgDataService {
    async fn create_payment_method(
        &self,
        store_id: Uuid,
        chain_id: &types::ChainId,
        token_address: Option<&str>,
        asset_symbol: &str,
        decimals: u8,
        xpub: Option<&str>,
    ) -> RepositoryResult<StorePaymentMethod> {
        // A supplied key pins the method to it, creating the account wallet if
        // it is new. No key leaves `wallet_id` NULL, so the method follows the
        // store's resolution afterwards rather than freezing today's answer -
        // which is the point: rotate the store's wallet and every unpinned
        // method moves with it.
        // A key supplied here is a key for THIS method's chain family: the
        // merchant is pasting it against a chain they picked. Filing it under
        // anything else - an account default, say - is the wrong-coin-type bug
        // reached through the payment-method form.
        let namespace = chain_id.namespace();

        let wallet_id = match xpub {
            Some(xpub) => Some(
                self.wallet_for_store_xpub(store_id, namespace, xpub)
                    .await?,
            ),
            None => {
                // Refuse now rather than at the first invoice. An unpinned
                // method on a store that resolves to nothing looks fine in the
                // list and fails only when a customer is waiting to pay. Asked
                // per family: an account with an Ethereum key and no Tron one
                // resolves for one chain and not the other, and the answer
                // that matters is the one for the chain being enabled.
                if !self.store_resolves_to_a_wallet(store_id, namespace).await? {
                    return Err(RepositoryError::Conflict(format!(
                        "This store has no receiving key for {namespace} chains to derive \
                         addresses from. Add one on the Wallets page, or supply an xpub \
                         with this payment method."
                    )));
                }
                None
            }
        };

        // Two conflict targets, because the table needs both. The composite
        // unique index cannot see native assets - token_address is NULL and
        // NULL is distinct from NULL - so those rely on the partial index
        // added alongside account wallets. Naming the wrong one silently
        // inserts a duplicate
        // instead of updating, which is how one store ended up with several
        // ETH methods, each formerly with its own counter.
        let sql = if token_address.is_some() {
            "INSERT INTO store_payment_methods
                 (store_id, chain_id, token_address, asset_symbol, decimals, wallet_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (store_id, chain_id, token_address) DO UPDATE
             SET wallet_id = $6, asset_symbol = $4, decimals = $5, enabled = true
             RETURNING id"
        } else {
            "INSERT INTO store_payment_methods
                 (store_id, chain_id, token_address, asset_symbol, decimals, wallet_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (store_id, chain_id) WHERE token_address IS NULL DO UPDATE
             SET wallet_id = $6, asset_symbol = $4, decimals = $5, enabled = true
             RETURNING id"
        };

        let id: Uuid = sqlx::query(sql)
            .bind(store_id)
            .bind(chain_id.as_str())
            .bind(token_address)
            .bind(asset_symbol)
            .bind(decimals as i16)
            .bind(wallet_id)
            .fetch_one(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?
            .get("id");

        // Re-read by the id the write returned, not by (store, chain, token).
        // The xpub and index the caller expects come from `wallets` and
        // `INSERT ... RETURNING` cannot reach across the join - but looking the
        // row back up by its natural key could return a different row than the
        // one just written wherever that key is not actually unique.
        self.get_payment_method(id)
            .await?
            .ok_or_else(|| RepositoryError::NotFound("payment method not found".into()))
    }

    async fn update_payment_method(
        &self,
        id: Uuid,
        enabled: Option<bool>,
        xpub: Option<&str>,
    ) -> RepositoryResult<StorePaymentMethod> {
        // Resolving the xpub to a wallet needs the store, so the method has to
        // exist first. This also gives `update` its not-found error rather
        // than letting a zero-row UPDATE report it later.
        let existing = self
            .get_payment_method(id)
            .await?
            .ok_or_else(|| RepositoryError::NotFound("payment method not found".into()))?;

        // The family comes from the method's own chain, which the update
        // cannot change - `UpdatePaymentMethodRequest` carries no chain id -
        // so a key pasted here is unambiguously a key for that chain.
        let wallet_id = match xpub {
            Some(x) => Some(
                self.wallet_for_store_xpub(existing.store_id, existing.chain_id.namespace(), x)
                    .await?,
            ),
            None => None,
        };

        // Setting an xpub pins the method to that wallet, overriding whatever
        // the store resolves to. It does NOT reset a counter, unlike the old
        // per-method xpub swap: the destination wallet already knows how far
        // its own key has been used, which is the whole reason the counter
        // moved. Resetting here would re-issue addresses.
        sqlx::query(
            r#"
            UPDATE store_payment_methods
            SET enabled = COALESCE($2, enabled),
                wallet_id = COALESCE($3, wallet_id)
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(enabled)
        .bind(wallet_id)
        .execute(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        self.get_payment_method(id)
            .await?
            .ok_or_else(|| RepositoryError::NotFound("payment method not found".into()))
    }

    async fn delete_payment_method(&self, id: Uuid) -> RepositoryResult<()> {
        let result = sqlx::query("DELETE FROM store_payment_methods WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(sqlx_to_repo_error)?;

        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound("payment method not found".into()));
        }

        Ok(())
    }

    async fn allocate_derivation(&self, id: Uuid) -> RepositoryResult<DerivationAllocation> {
        // Resolve, lock, advance and read the key back - one statement.
        //
        // The resolution is inside the UPDATE rather than done first, so the
        // wallet whose counter moves is by construction the wallet whose xpub
        // is returned. Reading the method (and its xpub) and then allocating
        // separately is a duplicate-address bug: a rotation or an override
        // change committing in between pairs one wallet's key with an index
        // consumed from another, and the first wallet never advances past that
        // index, so it hands the same address out again later.
        //
        // The row lock `UPDATE` takes on `wallets` also serialises concurrent
        // allocations, so two invoices on one wallet get different indices.
        let row = sqlx::query(&format!(
            r#"
            UPDATE wallets w
            SET derivation_index = w.derivation_index + 1
            FROM store_payment_methods pm
            JOIN stores s ON s.id = pm.store_id
            WHERE pm.id = $1
              AND w.id = {}
              AND w.namespace = {METHOD_NAMESPACE}
            RETURNING w.id AS wallet_id, w.namespace, w.xpub,
                      w.derivation_index - 1 AS current_index
            "#,
            method_wallet()
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        match row {
            Some(r) => Ok(DerivationAllocation {
                wallet_id: r.get("wallet_id"),
                namespace: r.get("namespace"),
                xpub: r.get("xpub"),
                index: r.get("current_index"),
            }),
            // Either the method is gone, or resolution found nothing for this
            // method's chain family: no pin, no store override, no account
            // primary in that namespace. All of them mean there is no key to
            // derive from, and inventing one is not an option - a wallet from
            // another family would derive a valid address at the wrong coin
            // type, which is worse than refusing because it looks like success.
            None => Err(RepositoryError::NotFound(
                "payment method has no wallet in its chain family to derive from".into(),
            )),
        }
    }
}
