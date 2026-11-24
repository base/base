//! Snap sync: downloads the account, storage and bytecode state at a recent pivot block from
//! snap/2 peers, verifies every range against the pivot's state root, and writes it to the
//! store while tiger executes blocks statelessly.
//!
//! Healing without trie nodes: the node applies every executed block's state changes to the
//! flat hashed tables for all keys, and reports the touched keys here
//! ([`SnapHandle::on_block`]). A range downloaded at pivot P is written without the keys that
//! blocks after P touched, because the table already holds their newer value (or their
//! deletion). Every other key in the range is unchanged since P, so the flat state stays
//! correct at the head. When every range is done, [`SnapStateSink::finish_snap`] rebuilds the
//! tries and checks the head's state root, which verifies the whole result.
//!
//! The pivot follows the newest executed block and moves when it falls more than
//! [`SnapSyncConfig::max_pivot_age`] blocks behind, since base-reth serves only the last 128
//! roots. Work already downloaded at an older pivot stays valid; only requests move on.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, mpsc::RecvTimeoutError},
    time::{Duration, Instant},
};

use alloy_primitives::{B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use alloy_trie::{EMPTY_ROOT_HASH, TrieAccount};

use crate::{
    crypto::PeerId,
    network::{Network, Penalty, response_channel},
    peer::{Peer, Response},
    range_proof::verify_range_proof,
    snap::{
        self, AccountRange, ByteCodes, GetAccountRange, GetByteCodes, GetStorageRanges, MAX_HASH,
        SnapAccount, SnapBatch, SnapRange, SnapStateSink, StorageRanges, StoreError,
    },
};

/// Snap sync limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapSyncConfig {
    /// Soft response size asked of peers.
    pub response_bytes: u64,
    /// How long a peer has to answer.
    pub request_timeout: Duration,
    /// Requests in flight per peer.
    pub max_inflight_per_peer: usize,
    /// The account key space is split into this many ranges fetched in parallel.
    pub account_chunks: usize,
    /// Most queued storage tries; account downloads pause when it is full.
    pub max_storage_tasks: usize,
    /// Most queued code hashes; account downloads pause when it is full.
    pub max_code_tasks: usize,
    /// Accounts whose storage is asked for in one request.
    pub storage_accounts_per_request: usize,
    /// Code hashes asked for in one request.
    pub codes_per_request: usize,
    /// Move the pivot when it is this many blocks behind the newest executed block.
    pub max_pivot_age: u64,
    /// Commit downloaded data at least this often.
    pub commit_interval: Duration,
}

impl Default for SnapSyncConfig {
    fn default() -> Self {
        Self {
            response_bytes: 512 * 1024,
            request_timeout: Duration::from_secs(10),
            max_inflight_per_peer: 2,
            account_chunks: 16,
            max_storage_tasks: 16 * 1024,
            max_code_tasks: 16 * 1024,
            storage_accounts_per_request: 128,
            codes_per_request: 64,
            max_pivot_age: 64,
            commit_interval: Duration::from_secs(2),
        }
    }
}

/// A block whose state root snap requests target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Pivot {
    /// Block number.
    pub number: u64,
    /// The block's state root.
    pub state_root: B256,
}

/// The hashed keys one executed block changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TouchedKeys {
    /// Hashed addresses whose account changed, was created, or was deleted.
    pub accounts: Vec<B256>,
    /// (hashed address, hashed slot) pairs whose value changed.
    pub storage: Vec<(B256, B256)>,
    /// Hashed addresses whose whole storage was wiped.
    pub wiped: Vec<B256>,
}

/// Where snap sync stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapStatus {
    /// Waiting for the first executed block to choose a pivot.
    WaitingForPivot,
    /// Downloading.
    Downloading {
        /// Fraction of the account key space done, in parts per thousand.
        accounts_per_mille: u32,
        /// Storage tries queued or in progress.
        storage_pending: usize,
        /// Code hashes queued.
        codes_pending: usize,
    },
    /// Every range is written and the store verified the root.
    Complete,
}

/// Why snap sync stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapSyncError {
    /// The store failed.
    Store(StoreError),
    /// The progress blob from the store does not decode.
    BadProgress,
}

impl From<StoreError> for SnapSyncError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Touched-key bookkeeping shared with the execution thread.
#[derive(Debug, Default)]
struct Touched {
    latest: Option<Pivot>,
    accounts: HashMap<B256, u64>,
    slots: HashMap<(B256, B256), u64>,
    wiped: HashMap<B256, u64>,
}

impl Touched {
    fn account_after(&self, account: &B256, pivot: u64) -> bool {
        self.accounts.get(account).is_some_and(|n| *n > pivot)
    }

    fn slot_after(&self, account: &B256, slot: &B256, pivot: u64) -> bool {
        self.wiped.get(account).is_some_and(|n| *n > pivot) ||
            self.slots.get(&(*account, *slot)).is_some_and(|n| *n > pivot)
    }

    /// Forgets touches at or below `pivot`: no outstanding or future request is older.
    fn prune(&mut self, pivot: u64) {
        self.accounts.retain(|_, n| *n > pivot);
        self.slots.retain(|_, n| *n > pivot);
        self.wiped.retain(|_, n| *n > pivot);
    }
}

/// The handle the execution side uses to report executed blocks.
#[derive(Debug, Clone)]
pub struct SnapHandle {
    touched: Arc<Mutex<Touched>>,
}

impl SnapHandle {
    /// Reports an executed block: its number and state root, and the keys it changed. Call it
    /// after the block's changes are in the flat tables, in block order.
    pub fn on_block(&self, number: u64, state_root: B256, touched: &TouchedKeys) {
        let mut state = self.touched.lock().unwrap();
        for account in &touched.accounts {
            state.accounts.insert(*account, number);
        }
        for slot in &touched.storage {
            state.slots.insert(*slot, number);
        }
        for account in &touched.wiped {
            state.wiped.insert(*account, number);
        }
        state.latest = Some(Pivot { number, state_root });
    }
}

/// One slice of the account key space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
struct AccountChunk {
    next: B256,
    last: B256,
    done: bool,
}

/// One account whose storage is still missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
struct StorageTask {
    account: B256,
    storage_root: B256,
    next: B256,
    pivot: Pivot,
}

/// Persisted progress: what is left. Everything else is in the store.
#[derive(Debug, Clone, Default, PartialEq, Eq, RlpEncodable, RlpDecodable)]
struct Progress {
    chunks: Vec<AccountChunk>,
    storage: Vec<StorageTask>,
    codes: Vec<B256>,
}

enum Request {
    Accounts { chunk: usize, pivot: Pivot, start: B256 },
    Storage { tasks: Vec<StorageTask> },
    Refresh { task: StorageTask, pivot: Pivot },
    Codes { hashes: Vec<B256> },
}

/// Downloads state with snap/2. Run [`SnapSync::run`] on its own thread.
pub struct SnapSync<S: SnapStateSink> {
    network: Network,
    sink: S,
    config: SnapSyncConfig,
    touched: Arc<Mutex<Touched>>,
    pivot: Option<Pivot>,
    progress: Progress,
    /// Storage tasks handed out, keyed by account, persisted with the progress.
    storage_busy: HashMap<B256, StorageTask>,
    chunk_busy: Vec<bool>,
    codes_busy: HashSet<B256>,
    batch: SnapBatch,
    last_commit: Instant,
}

impl<S: SnapStateSink> std::fmt::Debug for SnapSync<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapSync").field("pivot", &self.pivot).finish()
    }
}

fn account_rlp(account: &TrieAccount) -> Vec<u8> {
    alloy_rlp::encode(account)
}

impl<S: SnapStateSink> SnapSync<S> {
    /// Creates the syncer, resuming from the store's progress.
    pub fn new(network: Network, sink: S, config: SnapSyncConfig) -> Result<Self, SnapSyncError> {
        assert!(config.account_chunks > 0 && config.account_chunks <= 256);
        let progress = match sink.load_progress()? {
            Some(blob) => Progress::decode(&mut blob.as_slice()).map_err(|_| SnapSyncError::BadProgress)?,
            None => {
                let mut chunks = Vec::with_capacity(config.account_chunks);
                let step = U256::MAX / U256::from(config.account_chunks);
                for i in 0..config.account_chunks {
                    let next = B256::from(step * U256::from(i) + U256::from((i > 0) as u8));
                    let last = if i + 1 == config.account_chunks {
                        MAX_HASH
                    } else {
                        B256::from(step * U256::from(i + 1))
                    };
                    chunks.push(AccountChunk { next, last, done: false });
                }
                Progress { chunks, storage: Vec::new(), codes: Vec::new() }
            }
        };
        let chunk_busy = vec![false; progress.chunks.len()];
        Ok(Self {
            network,
            sink,
            config,
            touched: Arc::default(),
            pivot: None,
            progress,
            storage_busy: HashMap::new(),
            chunk_busy,
            codes_busy: HashSet::new(),
            batch: SnapBatch::default(),
            last_commit: Instant::now(),
        })
    }

    /// The handle for reporting executed blocks.
    pub fn handle(&self) -> SnapHandle {
        SnapHandle { touched: self.touched.clone() }
    }

    /// The current status.
    pub fn status(&self) -> SnapStatus {
        if self.is_done() {
            return SnapStatus::Complete;
        }
        if self.pivot.is_none() {
            return SnapStatus::WaitingForPivot;
        }
        // Remaining key space, measured on the top 64 bits of each key.
        let top = |h: &B256| u64::from_be_bytes(h.0[..8].try_into().unwrap()) as u128;
        let remaining: u128 = self
            .progress
            .chunks
            .iter()
            .filter(|c| !c.done)
            .map(|c| top(&c.last).saturating_sub(top(&c.next)))
            .sum();
        let done = (u64::MAX as u128).saturating_sub(remaining);
        SnapStatus::Downloading {
            accounts_per_mille: (done * 1000 / u64::MAX as u128) as u32,
            storage_pending: self.progress.storage.len() + self.storage_busy.len(),
            codes_pending: self.progress.codes.len() + self.codes_busy.len(),
        }
    }

    fn is_done(&self) -> bool {
        self.progress.chunks.iter().all(|c| c.done) &&
            self.progress.storage.is_empty() &&
            self.progress.codes.is_empty() &&
            self.storage_busy.is_empty() &&
            self.codes_busy.is_empty()
    }

    /// Downloads until every range is written, then asks the store to finish. Blocks.
    pub fn run(&mut self, progress: &mut dyn FnMut(SnapStatus)) -> Result<(), SnapSyncError> {
        let (tx, rx) = response_channel();
        let mut inflight: HashMap<u64, (Request, Arc<Peer>, Instant)> = HashMap::new();
        let mut next_tag = 0u64;
        let mut last_report = Instant::now();
        while !(self.is_done() && inflight.is_empty()) {
            self.update_pivot();
            // Dispatch.
            while let Some(peer) =
                self.pivot.and_then(|_| self.network.select_peer(true, self.config.max_inflight_per_peer, &[]))
            {
                let Some(request) = self.next_request() else { break };
                let tag = next_tag;
                next_tag += 1;
                if self.send(&peer, &request, tag, &tx) {
                    inflight.insert(tag, (request, peer, Instant::now()));
                } else {
                    self.release(request);
                    break;
                }
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Response { tag, peer, payload }) => {
                    if let Some((request, _, _)) = inflight.remove(&tag) {
                        match payload {
                            Some(payload) => self.on_response(request, &peer, &payload),
                            None => self.release(request),
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => unreachable!("we hold a sender"),
            }
            let now = Instant::now();
            let expired: Vec<u64> = inflight
                .iter()
                .filter(|(_, (_, _, sent))| now - *sent > self.config.request_timeout)
                .map(|(tag, _)| *tag)
                .collect();
            for tag in expired {
                let (request, peer, _) = inflight.remove(&tag).unwrap();
                peer.cancel_tag(tag);
                self.network.report(&peer.id, Penalty::Timeout);
                self.release(request);
            }
            if self.last_commit.elapsed() > self.config.commit_interval {
                self.commit()?;
            }
            if last_report.elapsed() > Duration::from_secs(1) {
                last_report = Instant::now();
                progress(self.status());
            }
        }
        self.commit()?;
        let head = self.touched.lock().unwrap().latest.expect("a pivot implies a reported block");
        self.sink.finish_snap(head.number, head.state_root)?;
        progress(SnapStatus::Complete);
        Ok(())
    }

    /// Follows the newest executed block; moves the pivot once it is too old to be served.
    fn update_pivot(&mut self) {
        let mut touched = self.touched.lock().unwrap();
        let Some(latest) = touched.latest else { return };
        let stale = self.pivot.is_none_or(|p| latest.number > p.number + self.config.max_pivot_age);
        if stale {
            self.pivot = Some(latest);
        }
        // Touches at or below the oldest pivot still referenced cannot matter any more.
        let oldest = self
            .progress
            .storage
            .iter()
            .map(|t| t.pivot.number)
            .chain(self.pivot.map(|p| p.number))
            .min()
            .unwrap_or(latest.number);
        // Keep a margin for requests still in flight at an older pivot.
        touched.prune(oldest.saturating_sub(self.config.max_pivot_age * 2));
    }

    fn next_request(&mut self) -> Option<Request> {
        let pivot = self.pivot?;
        if !self.progress.codes.is_empty() {
            let take = self.progress.codes.len().min(self.config.codes_per_request);
            let hashes: Vec<B256> = self.progress.codes.drain(..take).collect();
            self.codes_busy.extend(hashes.iter().copied());
            return Some(Request::Codes { hashes });
        }
        if !self.progress.storage.is_empty() {
            let first = self.progress.storage.remove(0);
            self.storage_busy.insert(first.account, first);
            // A task from an old pivot needs its account's storage root at the current one.
            if pivot.number > first.pivot.number + self.config.max_pivot_age {
                return Some(Request::Refresh { task: first, pivot });
            }
            let mut tasks = vec![first];
            // Batch whole small tries at the same pivot; a partial trie goes alone.
            if first.next == B256::ZERO {
                while tasks.len() < self.config.storage_accounts_per_request {
                    let Some(index) = self
                        .progress
                        .storage
                        .iter()
                        .position(|t| t.pivot == first.pivot && t.next == B256::ZERO)
                    else {
                        break;
                    };
                    let task = self.progress.storage.remove(index);
                    self.storage_busy.insert(task.account, task);
                    tasks.push(task);
                }
            }
            return Some(Request::Storage { tasks });
        }
        let queues_full = self.progress.storage.len() + self.storage_busy.len() >= self.config.max_storage_tasks ||
            self.progress.codes.len() + self.codes_busy.len() >= self.config.max_code_tasks;
        if queues_full {
            return None;
        }
        let chunk = (0..self.progress.chunks.len())
            .find(|i| !self.progress.chunks[*i].done && !self.chunk_busy[*i])?;
        self.chunk_busy[chunk] = true;
        Some(Request::Accounts { chunk, pivot, start: self.progress.chunks[chunk].next })
    }

    /// Puts a request's work back in the queues (failed, timed out, or peer gone).
    fn release(&mut self, request: Request) {
        match request {
            Request::Accounts { chunk, .. } => self.chunk_busy[chunk] = false,
            Request::Storage { tasks } => {
                for task in tasks {
                    self.storage_busy.remove(&task.account);
                    self.progress.storage.push(task);
                }
            }
            Request::Refresh { task, .. } => {
                self.storage_busy.remove(&task.account);
                self.progress.storage.push(task);
            }
            Request::Codes { hashes } => {
                for hash in hashes {
                    self.codes_busy.remove(&hash);
                    self.progress.codes.push(hash);
                }
            }
        }
    }

    fn send(&self, peer: &Peer, request: &Request, tag: u64, tx: &std::sync::mpsc::Sender<Response>) -> bool {
        let bytes = self.config.response_bytes;
        let result = match request {
            Request::Accounts { chunk, pivot, start } => {
                let last = self.progress.chunks[*chunk].last;
                peer.request_snap(
                    snap::GET_ACCOUNT_RANGE,
                    |id| {
                        alloy_rlp::encode(GetAccountRange {
                            request_id: id,
                            root: pivot.state_root,
                            start: *start,
                            limit: last,
                            response_bytes: bytes,
                        })
                    },
                    tag,
                    tx,
                )
            }
            Request::Refresh { task, pivot } => peer.request_snap(
                snap::GET_ACCOUNT_RANGE,
                |id| {
                    alloy_rlp::encode(GetAccountRange {
                        request_id: id,
                        root: pivot.state_root,
                        start: task.account,
                        limit: task.account,
                        response_bytes: 4096,
                    })
                },
                tag,
                tx,
            ),
            Request::Storage { tasks } => peer.request_snap(
                snap::GET_STORAGE_RANGES,
                |id| {
                    GetStorageRanges {
                        request_id: id,
                        root: tasks[0].pivot.state_root,
                        accounts: tasks.iter().map(|t| t.account).collect(),
                        start: tasks[0].next,
                        limit: MAX_HASH,
                        response_bytes: bytes,
                    }
                    .encode_payload()
                },
                tag,
                tx,
            ),
            Request::Codes { hashes } => peer.request_snap(
                snap::GET_BYTE_CODES,
                |id| alloy_rlp::encode(GetByteCodes { request_id: id, hashes: hashes.clone(), response_bytes: bytes }),
                tag,
                tx,
            ),
        };
        result.is_ok()
    }

    fn on_response(&mut self, request: Request, peer: &PeerId, payload: &[u8]) {
        let outcome = match &request {
            Request::Accounts { chunk, pivot, start } => self.on_accounts(*chunk, *pivot, *start, payload),
            Request::Refresh { task, pivot } => self.on_refresh(*task, *pivot, payload),
            Request::Storage { tasks } => self.on_storage(tasks, payload),
            Request::Codes { hashes } => self.on_codes(hashes, payload),
        };
        match outcome {
            Ok(()) => {}
            Err(penalty) => {
                self.network.report(peer, penalty);
                self.release(request);
            }
        }
    }

    fn on_accounts(&mut self, chunk: usize, pivot: Pivot, start: B256, payload: &[u8]) -> Result<(), Penalty> {
        let response = AccountRange::decode_payload(payload).map_err(|_| Penalty::BadResponse)?;
        if response.accounts.is_empty() && response.proof.is_empty() {
            // The peer does not have this root (too old, or not reached yet).
            return Err(Penalty::Useless);
        }
        let last_key = self.progress.chunks[chunk].last;
        let leaves: Vec<(B256, Vec<u8>)> =
            response.accounts.iter().map(|(hash, account)| (*hash, account_rlp(account))).collect();
        let outcome = verify_range_proof(pivot.state_root, start, last_key, &leaves, &response.proof)
            .map_err(|_| Penalty::BadResponse)?;
        let touched = self.touched.lock().unwrap();
        let mut reached_end = !outcome.more;
        let mut last_written = None;
        for (hash, account) in &response.accounts {
            if *hash > last_key {
                reached_end = true;
                break;
            }
            last_written = Some(*hash);
            if !touched.account_after(hash, pivot.number) {
                self.batch.accounts.push((
                    *hash,
                    SnapAccount { nonce: account.nonce, balance: account.balance, code_hash: account.code_hash },
                ));
            }
            if account.storage_root != EMPTY_ROOT_HASH {
                self.batch.storage_pending.push(*hash);
                self.progress.storage.push(StorageTask {
                    account: *hash,
                    storage_root: account.storage_root,
                    next: B256::ZERO,
                    pivot,
                });
            }
            if account.code_hash != KECCAK256_EMPTY && !self.codes_busy.contains(&account.code_hash) &&
                !self.progress.codes.contains(&account.code_hash)
            {
                if let Ok(false) = self.sink.has_code(account.code_hash) {
                    self.progress.codes.push(account.code_hash);
                }
            }
        }
        drop(touched);
        let state = &mut self.progress.chunks[chunk];
        self.chunk_busy[chunk] = false;
        if reached_end {
            self.batch.completed.push(SnapRange { trie: None, from: start, to: last_key });
            state.done = true;
        } else {
            let last = last_written.expect("a response that does not reach the end has leaves");
            self.batch.completed.push(SnapRange { trie: None, from: start, to: last });
            match increment(last) {
                Some(next) => state.next = next,
                None => state.done = true,
            }
        }
        Ok(())
    }

    fn on_refresh(&mut self, task: StorageTask, pivot: Pivot, payload: &[u8]) -> Result<(), Penalty> {
        let response = AccountRange::decode_payload(payload).map_err(|_| Penalty::BadResponse)?;
        if response.accounts.is_empty() && response.proof.is_empty() {
            return Err(Penalty::Useless);
        }
        let leaves: Vec<(B256, Vec<u8>)> =
            response.accounts.iter().map(|(hash, account)| (*hash, account_rlp(account))).collect();
        verify_range_proof(pivot.state_root, task.account, task.account, &leaves, &response.proof)
            .map_err(|_| Penalty::BadResponse)?;
        self.storage_busy.remove(&task.account);
        match response.accounts.first() {
            Some((hash, account)) if *hash == task.account && account.storage_root != EMPTY_ROOT_HASH => {
                self.progress.storage.push(StorageTask { storage_root: account.storage_root, pivot, ..task });
            }
            // The account or its storage is gone at the new pivot: what remains comes from the
            // executed blocks' diffs.
            _ => self.batch.completed.push(SnapRange { trie: Some(task.account), from: task.next, to: MAX_HASH }),
        }
        Ok(())
    }

    fn on_storage(&mut self, tasks: &[StorageTask], payload: &[u8]) -> Result<(), Penalty> {
        let response = StorageRanges::decode_payload(payload).map_err(|_| Penalty::BadResponse)?;
        if response.slots.is_empty() {
            return Err(Penalty::Useless);
        }
        if response.slots.len() > tasks.len() {
            return Err(Penalty::BadResponse);
        }
        // Verify everything before writing anything.
        let mut verified = Vec::with_capacity(response.slots.len());
        for (index, slots) in response.slots.iter().enumerate() {
            let task = tasks[index];
            let last = index + 1 == response.slots.len();
            let leaves: Vec<(B256, Vec<u8>)> =
                slots.iter().map(|(hash, value)| (*hash, alloy_rlp::encode(value))).collect();
            let proof: &[Bytes] = if last { &response.proof } else { &[] };
            let outcome = verify_range_proof(task.storage_root, task.next, MAX_HASH, &leaves, proof)
                .map_err(|_| Penalty::BadResponse)?;
            verified.push(outcome.more);
        }
        let touched = self.touched.lock().unwrap();
        for (index, slots) in response.slots.iter().enumerate() {
            let task = tasks[index];
            for (slot, value) in slots {
                if !touched.slot_after(&task.account, slot, task.pivot.number) {
                    self.batch.storage.push((task.account, *slot, *value));
                }
            }
            self.storage_busy.remove(&task.account);
            let more = verified[index];
            let to = if more { slots.last().map(|(h, _)| *h) } else { Some(MAX_HASH) };
            if let Some(to) = to {
                self.batch.completed.push(SnapRange { trie: Some(task.account), from: task.next, to });
            }
            if more {
                let next = slots.last().and_then(|(h, _)| increment(*h)).unwrap_or(task.next);
                self.progress.storage.insert(0, StorageTask { next, ..task });
            }
        }
        drop(touched);
        // Tasks the peer did not reach go back in the queue.
        for task in &tasks[response.slots.len()..] {
            self.storage_busy.remove(&task.account);
            self.progress.storage.push(*task);
        }
        Ok(())
    }

    fn on_codes(&mut self, hashes: &[B256], payload: &[u8]) -> Result<(), Penalty> {
        let response = ByteCodes::decode(&mut &payload[..]).map_err(|_| Penalty::BadResponse)?;
        if response.codes.is_empty() {
            return Err(Penalty::Useless);
        }
        let wanted: HashSet<B256> = hashes.iter().copied().collect();
        let mut got = HashSet::new();
        for code in response.codes {
            let hash = keccak256(&code);
            if !wanted.contains(&hash) {
                return Err(Penalty::BadResponse);
            }
            if got.insert(hash) {
                self.batch.codes.push((hash, code));
            }
        }
        for hash in hashes {
            self.codes_busy.remove(hash);
            if !got.contains(hash) {
                self.progress.codes.push(*hash);
            }
        }
        Ok(())
    }

    /// Writes the batch and the progress in one store transaction.
    fn commit(&mut self) -> Result<(), SnapSyncError> {
        self.last_commit = Instant::now();
        // Work handed out but unanswered is persisted as still to do.
        // Chunk cursors advance only on answers, so busy chunks are already as they should be.
        let mut progress = self.progress.clone();
        progress.codes.extend(self.codes_busy.iter().copied());
        progress.storage.extend(self.storage_busy.values().copied());
        let mut blob = Vec::new();
        progress.encode(&mut blob);
        self.batch.progress = blob;
        self.sink.commit_snap(&self.batch)?;
        self.batch = SnapBatch::default();
        Ok(())
    }
}

/// `hash + 1`, or `None` at the top of the key space.
fn increment(hash: B256) -> Option<B256> {
    let value = U256::from_be_bytes(hash.0);
    value.checked_add(U256::from(1)).map(B256::from)
}
