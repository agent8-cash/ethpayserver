//! What a plugin's host imports actually do.
//!
//! The wasm side of this is in `payserver-plugin-host`: a plugin calls
//! `storage_query` or `invoice_create`, gets a length back, and copies the
//! answer out with `host_take`. This is the other end.
//!
//! That wasm-to-host wiring - a compiled guest module actually reaching this
//! layer through wasmtime, not just a Rust-level call to it - is not code in
//! this repository, so it cannot be shown in a diff here: the linker that
//! binds the `storage_query` import lives in `payserver-plugin-host::runtime`
//! (the crate this workspace pins by `rev` in the root `Cargo.toml`), and the
//! guest-side round trip is exercised end to end, through a real compiled
//! wasm module and a real `wasmtime::Linker`, by
//! `a_plugin_can_ask_the_host_a_question_and_read_the_answer` in that crate's
//! `runtime.rs` tests. Combined with this file's own tests against a real
//! `PluginCalls` and a real Postgres, the two repositories together cover the
//! whole path a plugin's write actually takes; neither alone does.
//!
//! Two capabilities, and they are not alike. Storage runs SQL on a
//! connection Postgres authenticated as *that plugin's* role, and the
//! database is what confines it. Invoicing has no such backstop - it writes
//! to the money path - so its confinement is that the plugin cannot name a
//! store. See [`PluginCalls::invoice_create`].
//!
//! # The storage contract
//!
//! Request:
//!
//! ```json
//! {"statements": [
//!   {"sql": "UPDATE subscriptions SET paid_until = $1::timestamptz WHERE account_id = $2",
//!    "params": ["2026-11-01T00:00:00Z", "acct-7"]}
//! ]}
//! ```
//!
//! Response:
//!
//! ```json
//! {"results": [{"rows_affected": "1", "rows": []}]}
//! ```
//!
//! Four rules, each of which exists for a specific reason:
//!
//! **One call is one transaction.** Every statement in `statements` runs in
//! order inside a single transaction, and the whole thing commits or none of
//! it does. That is not a convenience: billing has to advance `paid_until`
//! *and* record which invoice paid for it, and a crash between those two
//! either credits a merchant twice or loses their payment. A plugin cannot
//! hold a transaction open across calls, because a transaction that outlived
//! a call could hold locks until its deadline with nothing running.
//!
//! **Parameters are bound, never pasted.** Injection is close to moot inside
//! a plugin's own schema - the role cannot reach anything else - but binding
//! keeps one entry to genuinely one statement, since Postgres's extended
//! protocol will not run two.
//!
//! **Every value is a string, both directions.** Parameters arrive as
//! strings and the SQL says what they are (`$1::timestamptz`); results must
//! be text and the SQL says so (`paid_until::text`). The symmetry is not
//! aesthetic. Amounts are `NUMERIC(38,18)`, and a JSON number is an IEEE-754
//! double: `129.000000000000000000` does not survive the round trip. Making
//! every value a string removes the question rather than answering it
//! per-type, and Postgres renders `NUMERIC` to text exactly.
//!
//! A column that is not text is an error naming the column and telling the
//! author to cast it, rather than a silently lossy conversion.
//!
//! Note the shape of that check: it happens per row, so a query that returns
//! **no rows** never exercises it. An author testing against an empty table
//! will see an uncast query succeed and the same query fail once there is
//! data. That is inherent - there is nothing to convert in zero rows - and it
//! is called out here so it reads as a documented edge rather than a bug
//! discovered in production.
//!
//! **The answer is capped.** A plugin can write `SELECT * FROM everything`,
//! and without a limit the host would materialise it, serialise it, and copy
//! it into wasm memory.

use std::sync::Arc;

use payserver_plugin_api::PluginId;
use payserver_plugin_host::PluginHostCalls;

use super::pools::PluginPools;

mod invoicing;
mod sql;
mod standing;
mod volume;

pub use invoicing::DeferredIssuer;
pub use standing::DeferredStanding;
pub use volume::{DeferredBulkVolume, DeferredVolume};

/// The late-bound host capabilities a plugin's imports resolve through.
///
/// One struct rather than one parameter per cell, because these are threaded
/// from `server.rs` through three layers of boot before they reach a plugin,
/// and a fourth capability should be a field here rather than a fourth
/// argument at every layer. Each cell is still published independently: they
/// become available at different moments and under different conditions.
#[derive(Clone, Default, Debug)]
pub struct DeferredCapabilities {
    /// Capability 3: issuing an invoice on the instance's own store.
    pub issuer: DeferredIssuer,
    /// Capability 6: reading what an account settled over a window.
    pub volume: DeferredVolume,
    /// Capability 6, batched. Published independently of `volume`: nothing
    /// requires an instance that can answer for one account to answer for
    /// many, even though in practice one implies the other.
    pub bulk_volume: DeferredBulkVolume,
    /// Capability 7: reading the standing the host stored for an account.
    pub standing: DeferredStanding,
}

/// One plugin's host imports: its database, and whether it may invoice.
pub struct PluginCalls {
    plugin: PluginId,
    pools: Arc<PluginPools>,
    /// Who issues an invoice when this plugin asks for one, if anything
    /// does.
    ///
    /// Unpublished is the ordinary case and not a degraded one: issuing
    /// requires an own store to issue on, and an instance that has not been
    /// told which store is its own has no honest answer to give. Absent
    /// means the plugin is told invoicing is unavailable, rather than the
    /// host guessing at a store.
    issuer: DeferredIssuer,
    /// Who answers when this plugin asks what an account settled, if
    /// anything does. Unpublished means the plugin is told so, rather than
    /// being handed a zero it would read as "this merchant sold nothing".
    volume: DeferredVolume,
    /// The batched form of the above, with the same rule: unpublished is
    /// reported as unavailable rather than answered with an empty list, which
    /// a plugin would read as "none of these merchants sold anything".
    bulk_volume: DeferredBulkVolume,
    /// Who answers when this plugin asks for an account's stored standing.
    /// Unpublished is reported as unavailable rather than as "no standing",
    /// which a plugin gating on it would read as permission to allow.
    standing: DeferredStanding,
    /// The runtime to drive the async database work on.
    ///
    /// [`PluginHostCalls`] is sync because the runtime calls plugins from
    /// inside `spawn_blocking`, so this runs on a blocking thread and
    /// `block_on` is legal there. Captured at construction rather than looked
    /// up per call, because the blocking thread this ends up on is not itself
    /// inside the runtime and `Handle::current()` would fail there.
    handle: tokio::runtime::Handle,
}

impl PluginCalls {
    /// # Panics
    /// If constructed outside a tokio runtime.
    #[must_use]
    pub fn new(plugin: PluginId, pools: Arc<PluginPools>) -> Self {
        Self {
            plugin,
            pools,
            issuer: DeferredIssuer::default(),
            volume: DeferredVolume::default(),
            bulk_volume: DeferredBulkVolume::default(),
            standing: DeferredStanding::default(),
            handle: tokio::runtime::Handle::current(),
        }
    }

    /// Point this plugin's host calls at the instance's capabilities, which
    /// may not have been published yet.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: &DeferredCapabilities) -> Self {
        self.issuer = capabilities.issuer.clone();
        self.volume = capabilities.volume.clone();
        self.bulk_volume = capabilities.bulk_volume.clone();
        self.standing = capabilities.standing.clone();
        self
    }
}

impl PluginHostCalls for PluginCalls {
    fn invoice_create(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.invoice_create_impl(request)
    }

    fn merchant_volume(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.merchant_volume_impl(request)
    }

    fn merchant_volumes(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.merchant_volumes_impl(request)
    }

    fn account_standing(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.account_standing_impl(request)
    }

    fn storage_query(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        self.storage_query_impl(request)
    }

    /// Refused: this host does not yet reach an account on a plugin's behalf.
    /// The trait requires the method so that an implementation cannot hand out
    /// a merchant-reaching capability by forgetting it; refusing is the
    /// explicit choice, and the plugin is told so rather than left to assume
    /// the notice was delivered.
    fn account_notice(&self, _request: &[u8]) -> Result<Vec<u8>, String> {
        Err("account_notice is not available on this host".to_string())
    }
}

#[cfg(test)]
mod account_notice_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use payserver_plugin_api::PluginId;
    use payserver_plugin_host::PluginHostCalls;

    use super::PluginCalls;
    use crate::services::plugins::pools::PluginPools;

    /// A refusal that turned into `Ok` would tell the plugin a notice reached
    /// a merchant when this host has no way to deliver one.
    #[tokio::test]
    async fn account_notice_is_refused_not_acknowledged() {
        let pools = Arc::new(PluginPools::new("postgres://localhost/x".to_string(), 4));
        let calls = PluginCalls::new(PluginId::new("cash.random.t").unwrap(), pools);
        let err = calls.account_notice(b"{}").unwrap_err();
        assert!(err.contains("not available"), "{err}");
    }
}
