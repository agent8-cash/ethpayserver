//! Mock block source for integration testing.
//!
//! Allows injecting synthetic blocks, balances, native transfers, and ERC20 logs
//! to test the payment detection pipeline without real RPC connections.
//!
//! Uses shared interior state (`Arc`) so `Clone` copies share the same data.
//! This lets a test retain a handle for injection while `ChainMonitor` owns another.

use super::{BlockNotification, BlockSource, BlockStream, LogFilter, NativeTransfer, SourceStatus};
use crate::error::{EvmError, EvmResult};
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::{Block, BlockTransactions, Header};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock as SyncRwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, RwLock, broadcast};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

/// Shared inner state for the mock source.
struct Inner {
    chain_id: u64,
    current_block: AtomicU64,
    balances: RwLock<HashMap<Address, U256>>,
    native_transfers: RwLock<HashMap<u64, Vec<NativeTransfer>>>,
    logs: RwLock<HashMap<u64, Vec<alloy::rpc::types::Log>>>,
    /// Behind a `Mutex` (not the `tokio::sync::RwLock` used elsewhere) because
    /// `kill_connection` replaces it synchronously and `status()` - part of
    /// the `BlockSource` trait - is not async.
    block_tx: Mutex<broadcast::Sender<EvmResult<BlockNotification>>>,
    /// How many times `subscribe_blocks` has succeeded. Lets a test assert
    /// that a stalled monitor actually reconnected, not just that it kept
    /// running.
    subscribe_count: AtomicU64,
    /// One counter per RPC method, so a test can assert how an operation
    /// *scales* rather than only that it produced the right answer.
    ///
    /// Call volume is not a performance nicety here. Both detection paths are
    /// O(1) per block - one log filter, one block read - and that shape is
    /// invisible to every test that checks only whether a payment was found,
    /// which is how native detection stayed O(N) in open invoices for as long
    /// as it did.
    calls: Mutex<HashMap<&'static str, u64>>,
    /// Last status a `subscribe_blocks` attempt produced. Like the real
    /// `RpcBlockSource`, this only changes as a side effect of an actual
    /// subscribe attempt - not the instant `reachable` flips - so a test
    /// cannot fake recovery by setting this directly; it has to go through
    /// the same path a stalled watchdog does.
    status: Mutex<SourceStatus>,
    /// Whether the next `subscribe_blocks` attempt succeeds. Separate from
    /// `status` so a test can simulate the endpoint going down and coming
    /// back independently of when the monitor notices.
    reachable: AtomicBool,
    /// Whether the calls block processing makes should block forever instead
    /// of returning. Simulates an RPC call made *from inside* block processing
    /// that never completes - as distinct from `kill_connection`, which kills
    /// the block stream but leaves ordinary request/response calls answering.
    ///
    /// Covers every call `process_block` issues rather than one of them, so a
    /// test wedging the loop does not silently stop wedging anything when
    /// detection changes which RPC it uses. That is exactly what happened when
    /// it gated `get_balance` alone.
    hung: AtomicBool,
    hang_notify: Notify,
    /// Hash of every block pushed so far, keyed by number. Backs
    /// `get_block_hash`, which reorg detection uses to check chain
    /// continuity across a gap.
    block_hashes: SyncRwLock<HashMap<u64, B256>>,
    /// Transaction hashes contained in each block, keyed by number. Backs
    /// `get_block`, which reorg re-validation falls back to when nothing is
    /// watched: the address-filtered scan has nothing to filter *for*, but
    /// the chain's own block contents don't depend on any address being
    /// watched. Absent from this map but present in `block_hashes` means a
    /// pushed block with no registered transactions, not an unknown block.
    block_tx_hashes: SyncRwLock<HashMap<u64, Vec<B256>>>,
    /// When set, `find_native_transfers_to` returns this error instead of
    /// looking anything up. Lets a test simulate an RPC failure during reorg
    /// re-validation without disturbing ordinary payment detection.
    find_native_transfers_error: SyncRwLock<Option<String>>,
    /// When set, `get_block_hash` returns this error instead of looking
    /// anything up. Lets a test simulate an RPC failure during the
    /// block-gap continuity check without disturbing ordinary processing.
    get_block_hash_error: SyncRwLock<Option<String>>,
}

/// A mock block source for testing payment detection.
///
/// Cloning produces a handle to the **same** underlying state, so data injected
/// through one handle is visible from the other. This is essential because
/// `ChainMonitor::new` takes ownership of the source while the test code
/// needs to inject blocks and balances after the monitor starts.
///
/// # Usage
/// ```ignore
/// let source = MockBlockSource::new(1);
/// let test_handle = source.clone(); // same state
/// let monitor = ChainMonitor::new(chain_config, source, config);
/// // test_handle.set_balance(...), test_handle.push_block(...)
/// ```
#[derive(Clone)]
pub struct MockBlockSource {
    inner: Arc<Inner>,
}

impl MockBlockSource {
    /// Create a new mock source for the given chain ID.
    pub fn new(chain_id: u64) -> Self {
        let (block_tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                chain_id,
                current_block: AtomicU64::new(0),
                balances: RwLock::new(HashMap::new()),
                native_transfers: RwLock::new(HashMap::new()),
                logs: RwLock::new(HashMap::new()),
                block_tx: Mutex::new(block_tx),
                subscribe_count: AtomicU64::new(0),
                calls: Mutex::new(HashMap::new()),
                status: Mutex::new(SourceStatus::Connected),
                reachable: AtomicBool::new(true),
                hung: AtomicBool::new(false),
                hang_notify: Notify::new(),
                block_hashes: SyncRwLock::new(HashMap::new()),
                block_tx_hashes: SyncRwLock::new(HashMap::new()),
                find_native_transfers_error: SyncRwLock::new(None),
                get_block_hash_error: SyncRwLock::new(None),
            }),
        }
    }

    /// Number of times `subscribe_blocks` has been called on this source
    /// (through any clone, since they share state).
    pub fn subscribe_count(&self) -> u64 {
        self.inner.subscribe_count.load(Ordering::SeqCst)
    }

    /// How many times `method` has been called on this source.
    #[must_use]
    pub fn call_count(&self, method: &str) -> u64 {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(method)
            .copied()
            .unwrap_or(0)
    }

    /// Every method called so far, with counts. Handy in a failure message:
    /// "12 calls" is not actionable, "get_balance 12, get_logs 1" is.
    #[must_use]
    pub fn call_counts(&self) -> Vec<(&'static str, u64)> {
        let mut counts: Vec<(&'static str, u64)> = self
            .inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        counts.sort_unstable();
        counts
    }

    /// Forget every recorded call. Lets one test measure several scenarios
    /// without a fresh source and fresh wiring for each.
    pub fn reset_call_counts(&self) {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Block while [`Self::hang_rpc`] is in effect.
    ///
    /// Shared by every call `process_block` makes, so a test wedging the loop
    /// does not have to know which RPC the loop happens to be sitting in.
    async fn await_if_hung(&self) {
        loop {
            if !self.inner.hung.load(Ordering::SeqCst) {
                break;
            }
            // Register interest before re-checking, so a `release_hang` that
            // lands between the load above and this point isn't missed.
            let notified = self.inner.hang_notify.notified();
            if !self.inner.hung.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    }

    fn record_call(&self, method: &'static str) {
        *self
            .inner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(method)
            .or_insert(0) += 1;
    }

    /// Make `find_native_transfers_to` fail with `message` until cleared with
    /// `None`. Simulates an RPC error during reorg re-validation.
    pub fn set_find_native_transfers_error(&self, message: Option<&str>) {
        *self.inner.find_native_transfers_error.write().unwrap() = message.map(ToString::to_string);
    }

    /// Make `get_block_hash` fail with `message` until cleared with `None`.
    /// Simulates an RPC error during the block-gap continuity check.
    pub fn set_get_block_hash_error(&self, message: Option<&str>) {
        *self.inner.get_block_hash_error.write().unwrap() = message.map(ToString::to_string);
    }

    /// Push a block notification to all subscribers.
    pub fn push_block(&self, block: BlockNotification) {
        self.inner
            .current_block
            .store(block.number, Ordering::SeqCst);
        self.inner
            .block_hashes
            .write()
            .unwrap()
            .insert(block.number, block.hash);
        let _ = self
            .inner
            .block_tx
            .lock()
            .expect("mock block_tx mutex poisoned")
            .send(Ok(block));
    }

    /// Kill the RPC endpoint: every `subscribe_blocks` attempt fails (and
    /// reports `status() == Disconnected`, exactly like `RpcBlockSource`
    /// does on a failed connect) until [`Self::restore_connection`]. Also
    /// severs every stream already handed out, the way a dropped WebSocket
    /// would: an existing subscriber sees its stream end for good, and only
    /// a fresh, successful `subscribe_blocks` call sees blocks pushed after
    /// this point.
    pub fn kill_connection(&self) {
        self.inner.reachable.store(false, Ordering::SeqCst);
        let (new_tx, _) = broadcast::channel(256);
        *self
            .inner
            .block_tx
            .lock()
            .expect("mock block_tx mutex poisoned") = new_tx;
    }

    /// Restore the RPC endpoint. `status()` does not move back to
    /// `Connected` until something actually calls `subscribe_blocks` again -
    /// same as the real source, whose status is only ever touched inside a
    /// subscribe attempt - so recovery still depends on the monitor retrying
    /// on its own.
    pub fn restore_connection(&self) {
        self.inner.reachable.store(true, Ordering::SeqCst);
    }

    /// Set the balance for an address.
    pub async fn set_balance(&self, address: Address, balance: U256) {
        self.inner.balances.write().await.insert(address, balance);
    }

    /// Add a native transfer to a specific block.
    pub async fn add_native_transfer(&self, block_number: u64, transfer: NativeTransfer) {
        self.inner
            .native_transfers
            .write()
            .await
            .entry(block_number)
            .or_default()
            .push(transfer);
    }

    /// Add an ERC20 Transfer log to a specific block.
    pub async fn add_log(&self, block_number: u64, log: alloy::rpc::types::Log) {
        self.inner
            .logs
            .write()
            .await
            .entry(block_number)
            .or_default()
            .push(log);
    }

    /// Set the current block number without pushing a notification.
    pub fn set_block_number(&self, number: u64) {
        self.inner.current_block.store(number, Ordering::SeqCst);
    }

    /// Make the RPC calls `process_block` issues block forever, simulating a
    /// call made from inside the monitor's own event loop that never returns -
    /// wedging the loop itself, rather than just leaving its subscription
    /// silent the way [`Self::kill_connection`] does.
    ///
    /// Covers every call `process_block` issues - the block read, the log
    /// query, the balance read and the reorg continuity check - because which
    /// one the loop is sitting in is an implementation detail of detection and
    /// not what a test wedging the loop is trying to say. Gating only
    /// `get_balance` meant these tests quietly stopped wedging anything when
    /// native detection moved to reading the block instead of polling; gating
    /// only the two calls native detection happens to make now would leave the
    /// same trap for an ERC20-only or reorg-path test.
    pub fn hang_rpc(&self) {
        self.inner.hung.store(true, Ordering::SeqCst);
    }

    /// Release every call currently blocked by [`Self::hang_rpc`] (and let
    /// future ones return normally).
    pub fn release_hang(&self) {
        self.inner.hung.store(false, Ordering::SeqCst);
        self.inner.hang_notify.notify_waiters();
    }

    /// Directly set the canonical hash the mock reports for a given height,
    /// without pushing a block notification.
    ///
    /// Lets a test simulate a reorg that replaced a block the monitor has
    /// already processed: `push_block` alone cannot express "block N now has
    /// a different hash" without also moving the current block forward.
    pub fn set_block_hash(&self, number: u64, hash: B256) {
        self.inner
            .block_hashes
            .write()
            .unwrap()
            .insert(number, hash);
    }

    /// Set the transaction hashes `get_block` reports for block `number`,
    /// independent of any watched address.
    ///
    /// Lets a test simulate what a reorg actually left at a height -
    /// including "nothing", for a transaction the reorg genuinely dropped -
    /// without going through `add_native_transfer`/`add_log`, which are
    /// address-filtered and so cannot express a transaction's presence on
    /// its own.
    pub fn set_block_transactions(&self, number: u64, tx_hashes: Vec<B256>) {
        self.inner
            .block_tx_hashes
            .write()
            .unwrap()
            .insert(number, tx_hashes);
    }
}

#[async_trait]
impl BlockSource for MockBlockSource {
    fn chain_id(&self) -> u64 {
        self.inner.chain_id
    }

    fn status(&self) -> SourceStatus {
        self.inner
            .status
            .lock()
            .expect("mock status mutex poisoned")
            .clone()
    }

    async fn subscribe_blocks(&self) -> EvmResult<BlockStream> {
        if !self.inner.reachable.load(Ordering::SeqCst) {
            *self
                .inner
                .status
                .lock()
                .expect("mock status mutex poisoned") = SourceStatus::Disconnected;
            return Err(EvmError::Connection(
                "mock source is not connected".to_string(),
            ));
        }
        *self
            .inner
            .status
            .lock()
            .expect("mock status mutex poisoned") = SourceStatus::Connected;

        self.inner.subscribe_count.fetch_add(1, Ordering::SeqCst);
        let rx = self
            .inner
            .block_tx
            .lock()
            .expect("mock block_tx mutex poisoned")
            .subscribe();
        let stream = BroadcastStream::new(rx).filter_map(|result| match result {
            Ok(Ok(block)) => Some(Ok(block)),
            Ok(Err(e)) => Some(Err(e)),
            Err(_) => None, // Lagged receiver, skip
        });
        Ok(Box::pin(stream))
    }

    async fn get_logs(&self, filter: &LogFilter) -> EvmResult<Vec<alloy::rpc::types::Log>> {
        self.record_call("get_logs");
        self.await_if_hung().await;
        let logs = self.inner.logs.read().await;
        let mut result = Vec::new();

        let from = filter.from_block.unwrap_or(0);
        let to = filter.to_block.unwrap_or(u64::MAX);

        for block_num in from..=to {
            if let Some(block_logs) = logs.get(&block_num) {
                for log in block_logs {
                    if !filter.topics.is_empty() {
                        let matches = filter.topics.iter().enumerate().all(|(i, topic_filter)| {
                            match topic_filter {
                                Some(expected) => log.topics().get(i) == Some(expected),
                                None => true,
                            }
                        });
                        if !matches {
                            continue;
                        }
                    }
                    result.push(log.clone());
                }
            }
        }

        Ok(result)
    }

    async fn get_balance(&self, address: Address, _block: Option<u64>) -> EvmResult<U256> {
        self.record_call("get_balance");
        self.await_if_hung().await;

        let balances = self.inner.balances.read().await;
        Ok(balances.get(&address).copied().unwrap_or(U256::ZERO))
    }

    async fn get_block_number(&self) -> EvmResult<u64> {
        self.record_call("get_block_number");
        Ok(self.inner.current_block.load(Ordering::SeqCst))
    }

    async fn get_block(&self, number: u64) -> EvmResult<Option<Block>> {
        self.record_call("get_block");
        let Some(hash) = self
            .inner
            .block_hashes
            .read()
            .unwrap()
            .get(&number)
            .copied()
        else {
            return Ok(None);
        };
        let tx_hashes = self
            .inner
            .block_tx_hashes
            .read()
            .unwrap()
            .get(&number)
            .cloned()
            .unwrap_or_default();
        let header = Header {
            hash,
            inner: alloy::consensus::Header {
                number,
                ..Default::default()
            },
            total_difficulty: None,
            size: None,
        };
        Ok(Some(Block::new(
            header,
            BlockTransactions::Hashes(tx_hashes),
        )))
    }

    async fn get_block_hash(&self, number: u64) -> EvmResult<Option<B256>> {
        self.await_if_hung().await;
        if let Some(message) = self.inner.get_block_hash_error.read().unwrap().clone() {
            return Err(EvmError::Rpc(message));
        }

        Ok(self
            .inner
            .block_hashes
            .read()
            .unwrap()
            .get(&number)
            .copied())
    }

    async fn find_native_transfers_to(
        &self,
        block_number: u64,
        addresses: &[Address],
    ) -> EvmResult<Vec<NativeTransfer>> {
        self.record_call("find_native_transfers_to");
        self.await_if_hung().await;
        if let Some(message) = self
            .inner
            .find_native_transfers_error
            .read()
            .unwrap()
            .clone()
        {
            return Err(EvmError::Rpc(message));
        }

        let transfers = self.inner.native_transfers.read().await;
        Ok(transfers
            .get(&block_number)
            .map(|txs| {
                txs.iter()
                    .filter(|tx| addresses.contains(&tx.to))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn is_healthy(&self) -> bool {
        true
    }
}

/// Create a synthetic block notification with random hashes.
pub fn make_block(number: u64) -> BlockNotification {
    make_block_with_parent(number, B256::random(), B256::random())
}

/// Create a synthetic block notification with specific hashes.
pub fn make_block_with_parent(number: u64, hash: B256, parent_hash: B256) -> BlockNotification {
    BlockNotification {
        number,
        hash,
        parent_hash,
        timestamp: 1700000000 + number * 12,
    }
}

/// Create a synthetic native transfer.
pub fn make_native_transfer(
    from: Address,
    to: Address,
    value: U256,
    tx_hash: B256,
) -> NativeTransfer {
    NativeTransfer {
        tx_hash,
        from,
        to,
        value,
        tx_index: 0,
    }
}

/// Build an ERC20 Transfer log for testing.
///
/// Encodes: `Transfer(from, to, amount)` at the given token contract address.
pub fn make_erc20_transfer_log(
    token_contract: Address,
    from: Address,
    to: Address,
    amount: U256,
    block_number: u64,
    tx_hash: B256,
    log_index: u64,
) -> alloy::rpc::types::Log {
    use alloy::primitives::LogData;

    // topic0 = keccak256("Transfer(address,address,uint256)")
    let transfer_topic: B256 = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        .parse()
        .expect("valid transfer topic");

    // Pad address (20 bytes) to 32 bytes, left-aligned with zeros
    let mut from_bytes = [0u8; 32];
    from_bytes[12..].copy_from_slice(from.as_slice());
    let from_topic = B256::from(from_bytes);

    let mut to_bytes = [0u8; 32];
    to_bytes[12..].copy_from_slice(to.as_slice());
    let to_topic = B256::from(to_bytes);
    let amount_bytes: [u8; 32] = amount.to_be_bytes();

    let log_data = LogData::new(
        vec![transfer_topic, from_topic, to_topic],
        amount_bytes.to_vec().into(),
    )
    .expect("valid log data");

    alloy::rpc::types::Log {
        inner: alloy::primitives::Log {
            address: token_contract,
            data: log_data,
        },
        block_hash: Some(B256::random()),
        block_number: Some(block_number),
        block_timestamp: Some(1700000000 + block_number * 12),
        transaction_hash: Some(tx_hash),
        transaction_index: Some(0),
        log_index: Some(log_index),
        removed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_source_basics() {
        let source = MockBlockSource::new(1);
        assert_eq!(source.chain_id(), 1);
        assert_eq!(source.status(), SourceStatus::Connected);
        assert!(source.is_healthy().await);
        assert_eq!(source.get_block_number().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_mock_source_clone_shares_state() {
        let source = MockBlockSource::new(1);
        let handle = source.clone();
        let addr = Address::random();

        source.set_balance(addr, U256::from(42)).await;
        assert_eq!(
            handle.get_balance(addr, None).await.unwrap(),
            U256::from(42)
        );
    }

    #[tokio::test]
    async fn test_mock_source_balance() {
        let source = MockBlockSource::new(1);
        let addr = Address::random();

        assert_eq!(source.get_balance(addr, None).await.unwrap(), U256::ZERO);
        source.set_balance(addr, U256::from(1_000_000)).await;
        assert_eq!(
            source.get_balance(addr, None).await.unwrap(),
            U256::from(1_000_000)
        );
    }

    #[tokio::test]
    async fn test_mock_source_native_transfers() {
        let source = MockBlockSource::new(1);
        let to_addr = Address::random();
        let transfer = make_native_transfer(
            Address::random(),
            to_addr,
            U256::from(1_000),
            B256::random(),
        );

        source.add_native_transfer(100, transfer).await;

        let found = source
            .find_native_transfers_to(100, &[to_addr])
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].to, to_addr);

        let found = source
            .find_native_transfers_to(100, &[Address::random()])
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn test_mock_source_block_stream() {
        let source = MockBlockSource::new(1);
        let mut stream = source.subscribe_blocks().await.unwrap();

        let block = make_block(42);
        source.push_block(block);

        let received = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(received.number, 42);
        assert_eq!(source.get_block_number().await.unwrap(), 42);
    }
}
