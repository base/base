//! A block's state root computed while the block executes: each transaction's committed changes
//! stream into the live trie ([`crate::LiveState`]) on a background thread, which reveals and
//! hashes them as they come, so little is left when the block ends.
//!
//! The streamed values are provisional. [`LiveSession::finish`] takes the block's final changes
//! (the authority) and reconciles: storage tries whose streamed changes do not match the final
//! ones are dropped and recomputed from the stored trie, account leaves the stream changed but
//! the block does not are restored, and then the final changes apply over what is left. So the
//! root and trie updates are those of the final changes over the parent state whatever was
//! streamed; streaming only decides how much work is already done.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};

use alloy_primitives::B256;
use base_tiger_store::{Datadir, Db, Ro, Txn};

use crate::{
    BlockChanges, HashedState, LiveSource, LiveState, LiveTrie, MemState, Originals, Sources,
    StorageTrieUpdates, TrieError, TrieUpdates, TrieView,
};

/// No changes, for sources that read a stored state as it is.
pub static NO_CHANGES: HashedState =
    HashedState { accounts: BTreeMap::new(), storages: BTreeMap::new() };

/// Changes smaller than this (accounts, storage tries and slots) apply on one thread: spawning
/// workers and opening their read transactions costs more than they save.
const PARALLEL_MIN_WORK: usize = 512;

/// Most threads a live root uses.
const LIVE_THREADS: usize = 16;

/// The live trie is dropped when it holds more nodes than this, and rebuilt from the stored trie.
const LIVE_MAX_NODES: usize = 4_000_000;

/// The state of a [`crate::StateDb`]'s live trie, shared with the sessions using it.
#[derive(Debug, Default)]
pub enum LiveSlot {
    /// None yet (or dropped): the next root starts one from the stored trie.
    #[default]
    Empty,
    /// Idle, holding the state with this root.
    Idle(B256, Box<LiveState>),
    /// A root is using it; other roots use the walker meanwhile.
    Busy,
}

/// Where a live root reads: the datadir as one read snapshot saw it, with the in-memory layer.
#[derive(Debug, Clone)]
pub struct LiveBase {
    /// The datadir's tables.
    pub db: Db,
    /// The in-memory canonical blocks over the datadir.
    pub mem: Option<Arc<MemState>>,
    /// The snapshot's transaction id, checked by every reader when there is no in-memory layer.
    pub snapshot: u64,
}

/// A [`LiveSource`] over its own read transaction.
struct WorkerSource<'b> {
    txn: Txn<Ro>,
    base: &'b LiveBase,
}

impl WorkerSource<'_> {
    fn view(&self) -> TrieView<'_, Ro> {
        TrieView::new(&self.txn, &self.base.db, &NO_CHANGES).with_mem(self.base.mem.as_deref())
    }
}

impl LiveSource for WorkerSource<'_> {
    fn branch(
        &mut self,
        owner: Option<B256>,
        path: &alloy_trie::Nibbles,
    ) -> Result<Option<alloy_trie::BranchNodeCompact>, TrieError> {
        self.view().branch(owner, path)
    }

    fn first_branch_under(
        &mut self,
        owner: Option<B256>,
        prefix: &alloy_trie::Nibbles,
    ) -> Result<Option<(alloy_trie::Nibbles, alloy_trie::BranchNodeCompact)>, TrieError> {
        self.view().first_branch_under(owner, prefix)
    }

    fn leaves(
        &mut self,
        owner: Option<B256>,
        prefix: &alloy_trie::Nibbles,
        out: &mut Vec<(B256, Vec<u8>)>,
    ) -> Result<(), TrieError> {
        self.view().leaves(owner, prefix, out)
    }
}

impl LiveBase {
    /// A source over a new read transaction that sees this base's state.
    fn source(&self) -> Result<WorkerSource<'_>, TrieError> {
        let txn = self.db.read()?;
        // The same state: the same snapshot, or one whose tip the in-memory layer covers.
        let same = match &self.mem {
            Some(mem) => Datadir::tip_in(&txn, &self.db)?.is_some_and(|tip| tip <= mem.head),
            None => txn.id() == self.snapshot,
        };
        if !same {
            return Err(TrieError::Corrupt("the stored state moved under a live root".into()));
        }
        Ok(WorkerSource { txn, base: self })
    }

    /// Applies `state` to `live` with a source per worker thread (see
    /// [`LiveState::apply_over`]).
    /// Small changes run on the calling thread with `local`.
    fn apply(
        &self,
        local: &mut WorkerSource<'_>,
        live: &mut LiveState,
        state: &HashedState,
        wiped: &BTreeSet<B256>,
        originals: Option<&Originals>,
    ) -> Result<(B256, TrieUpdates), TrieError> {
        let work = state.accounts.len()
            + state.storages.values().map(|s| 1 + s.slots.len()).sum::<usize>();
        if work < PARALLEL_MIN_WORK {
            return live.apply_over(&mut Sources::Local(local), state, wiped, originals);
        }
        let sources =
            || -> Result<Box<dyn LiveSource + '_>, TrieError> { Ok(Box::new(self.source()?)) };
        let threads = std::thread::available_parallelism().map_or(1, usize::from).min(LIVE_THREADS);
        live.apply_over(&mut Sources::Threads(&sources, threads), state, wiped, originals)
    }

    /// The kept live trie when it holds this base's state, else a new one.
    fn resume(
        &self,
        local: &WorkerSource<'_>,
        parent: Option<(B256, Box<LiveState>)>,
    ) -> Result<Box<LiveState>, TrieError> {
        Ok(match parent {
            Some((root, live)) if local.view().state_root(false)?.0 == root => live,
            _ => Box::default(),
        })
    }

    /// Whether `TIGER_LIVE_STATS=1` asks for a line per streamed block.
    fn stats_enabled() -> bool {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var("TIGER_LIVE_STATS").is_ok_and(|v| v == "1"))
    }
}

impl LiveSlot {
    /// Takes the live trie out of `slot` for one root; `None` when another root is using it.
    pub fn take(slot: &Mutex<Self>) -> Option<Option<(B256, Box<LiveState>)>> {
        match std::mem::replace(&mut *slot.lock().expect("unpoisoned"), Self::Busy) {
            Self::Busy => None,
            Self::Idle(root, live) => Some(Some((root, live))),
            Self::Empty => Some(None),
        }
    }

    /// Returns the live trie to `slot` after a root: kept at `root` when it succeeded and fits.
    pub fn put(slot: &Mutex<Self>, kept: Option<(B256, Box<LiveState>)>) {
        *slot.lock().expect("unpoisoned") = match kept {
            Some((root, live)) if live.len() <= LIVE_MAX_NODES => Self::Idle(root, live),
            _ => Self::Empty,
        };
    }
}

impl LiveBase {
    /// The root and trie updates after `changes`, from the live trie in `slot` over this base
    /// (see [`LiveSession`] for the streaming form). `None` when the live trie is in use or
    /// fails; the caller then uses the walker, whose answer is authoritative.
    pub fn root_once(
        &self,
        slot: &Mutex<LiveSlot>,
        changes: &HashedState,
    ) -> Option<(B256, TrieUpdates)> {
        let parent = LiveSlot::take(slot)?;
        let result = self.source().and_then(|mut local| {
            let mut live = self.resume(&local, parent)?;
            Ok((self.apply(&mut local, &mut live, changes, &BTreeSet::new(), None)?, live))
        });
        match result {
            Ok(((root, updates), live)) => {
                LiveSlot::put(slot, Some((root, live)));
                Some((root, updates))
            }
            Err(error) => {
                eprintln!("tiger: live state root failed, the walker answers: {error}");
                LiveSlot::put(slot, None);
                None
            }
        }
    }
}

enum Message {
    /// One executed transaction's changes.
    Transaction(BlockChanges),
    /// The block's final hashed changes; the answer goes back on the channel.
    Finish(HashedState, mpsc::Sender<Option<(B256, TrieUpdates)>>),
    /// Answered once everything sent before it is applied.
    Flush(mpsc::Sender<()>),
}

/// The streamed part of a session: the trie and what the stream did to it.
struct Stream {
    live: Box<LiveState>,
    /// Everything streamed, merged.
    streamed: HashedState,
    /// The account leaves before the stream first changed them.
    originals: Originals,
    /// Trie updates of the streamed changes so far.
    updates: TrieUpdates,
    /// What the session did, printed per block with `TIGER_LIVE_STATS=1`.
    stats: StreamStats,
}

/// Counts of one streaming session, for `TIGER_LIVE_STATS=1`.
#[derive(Debug, Default)]
struct StreamStats {
    /// The live trie was rebuilt from the stored trie (not the previous block's).
    fresh: bool,
    /// Streamed transactions, and the batches they were applied in.
    transactions: usize,
    batches: usize,
    /// Storage tries the final changes could not reach (recomputed), and slots set back to
    /// their parent values.
    dropped: usize,
    reverted: usize,
}

impl Stream {
    /// Applies one batch of streamed changes.
    fn batch(
        &mut self,
        base: &LiveBase,
        local: &mut WorkerSource<'_>,
        batch: &HashedState,
    ) -> Result<(), TrieError> {
        // A storage trie wiped earlier in the stream and since dropped from the live trie (its
        // account was deleted) starts empty when later transactions write it, not from the
        // stored trie.
        for (address, storage) in &batch.storages {
            if !storage.wiped && self.streamed.storages.get(address).is_some_and(|s| s.wiped) {
                self.live
                    .storages
                    .entry(*address)
                    .or_insert_with(|| LiveTrie::empty(Some(*address)));
            }
        }
        self.stats.batches += 1;
        let (_, updates) =
            base.apply(local, &mut self.live, batch, &BTreeSet::new(), Some(&self.originals))?;
        Self::merge(&mut self.updates, updates);
        self.streamed.extend(batch);
        Ok(())
    }

    /// Merges later trie updates into `into`: a wipe replaces a storage trie's earlier changes.
    fn merge(into: &mut TrieUpdates, later: TrieUpdates) {
        into.account_nodes.extend(later.account_nodes);
        for (address, storage) in later.storage_tries {
            match into.storage_tries.get_mut(&address) {
                Some(earlier) if !storage.wiped => earlier.nodes.extend(storage.nodes),
                _ => {
                    into.storage_tries.insert(address, storage);
                }
            }
        }
    }

    /// The root and trie updates of `fin` (the block's final changes) over the parent state.
    fn finish(
        mut self,
        base: &LiveBase,
        local: &mut WorkerSource<'_>,
        fin: &HashedState,
    ) -> Result<(B256, TrieUpdates, Box<LiveState>), TrieError> {
        // A slot the stream changed that the final changes leave out ended at its parent value
        // (the final changes drop such slots): it goes back to that value, so the trie stays
        // revealed. Storage tries the final changes cannot be reached from that way (wipes that
        // differ, slots only the final changes name, deleted accounts) are dropped and
        // recomputed from the stored trie.
        let started = std::time::Instant::now();
        let requested = fin;
        let mut fin = std::borrow::Cow::Borrowed(fin);
        let mut kept_wipes = BTreeSet::new();
        let mut restore: BTreeSet<B256> = BTreeSet::new();
        for (address, streamed) in &self.streamed.storages {
            let wanted = fin.storages.get(address);
            let wiped = wanted.is_some_and(|f| f.wiped);
            let reachable = wiped == streamed.wiped
                && wanted.is_none_or(|f| f.slots.keys().all(|k| streamed.slots.contains_key(k)))
                && (wanted.is_some() || !streamed.wiped)
                && !fin.accounts.get(address).is_some_and(Option::is_none);
            if !reachable {
                self.stats.dropped += 1;
                self.live.storages.remove(address);
                self.updates.storage_tries.remove(address);
                restore.insert(*address);
                continue;
            }
            if streamed.wiped {
                kept_wipes.insert(*address);
            }
            let reverted: Vec<B256> = streamed
                .slots
                .keys()
                .filter(|k| wanted.is_none_or(|f| !f.slots.contains_key(*k)))
                .copied()
                .collect();
            if reverted.is_empty() {
                continue;
            }
            self.stats.reverted += reverted.len();
            let mut parent = Vec::new();
            for slot in reverted {
                // A wiped trie's parent value is absent.
                let value = if wiped {
                    alloy_primitives::U256::ZERO
                } else {
                    parent.clear();
                    local.leaves(
                        Some(*address),
                        &alloy_trie::Nibbles::unpack(slot),
                        &mut parent,
                    )?;
                    match parent.first() {
                        Some((_, leaf)) => alloy_rlp::decode_exact(leaf)
                            .map_err(|error| TrieError::Corrupt(error.to_string()))?,
                        None => alloy_primitives::U256::ZERO,
                    }
                };
                fin.to_mut().storages.entry(*address).or_default().slots.insert(slot, value);
            }
        }
        // Account leaves the final changes do not fully determine go back to the parent's.
        for address in self.streamed.accounts.keys() {
            if !fin.accounts.contains_key(address) {
                restore.insert(*address);
            }
        }
        if !restore.is_empty() {
            let mut nodes = std::mem::take(&mut self.updates.account_nodes);
            let originals = self.originals.get_mut().expect("unpoisoned");
            for address in &restore {
                let original = originals.get(address).cloned().flatten();
                self.live.accounts.update(local, *address, original, &mut nodes)?;
            }
            self.updates.account_nodes = nodes;
        }
        let (root, updates) = base.apply(local, &mut self.live, &fin, &kept_wipes, None)?;
        if LiveBase::stats_enabled() {
            let stats = &self.stats;
            eprintln!(
                "tiger: live root fresh={} txs={} batches={} accounts={} storages={} dropped={} reverted={} finish_us={} nodes={}",
                stats.fresh,
                stats.transactions,
                stats.batches,
                requested.accounts.len(),
                requested.storages.len(),
                stats.dropped,
                stats.reverted,
                started.elapsed().as_micros(),
                self.live.len()
            );
        }
        Self::merge(&mut self.updates, updates);
        // Only the storage tries the final changes name, with their wipes.
        let mut out = TrieUpdates {
            account_nodes: self.updates.account_nodes,
            storage_tries: BTreeMap::new(),
        };
        for (address, storage) in self.updates.storage_tries {
            if let Some(f) = requested.storages.get(&address) {
                let nodes = if requested.accounts.get(&address).is_some_and(Option::is_none) {
                    BTreeMap::new()
                } else {
                    storage.nodes
                };
                out.storage_tries.insert(address, StorageTrieUpdates { wiped: f.wiped, nodes });
            }
        }
        Ok((root, out, self.live))
    }
}

/// A block's state root computed as it executes (see the module docs). Feed it each executed
/// transaction with [`Self::transaction`], then call [`Self::finish`] with the block's changes.
/// Dropping it unfinished discards the live trie.
#[derive(Debug)]
pub struct LiveSession {
    sender: Option<mpsc::Sender<Message>>,
    thread: Option<JoinHandle<()>>,
}

impl LiveSession {
    /// Starts a session over `base` with the live trie in `slot`; `None` when it is in use.
    pub fn start(slot: Arc<Mutex<LiveSlot>>, base: LiveBase) -> Option<Self> {
        let parent = LiveSlot::take(&slot)?;
        let (sender, receiver) = mpsc::channel::<Message>();
        let thread = std::thread::Builder::new()
            .name("live-root".into())
            .spawn(move || Self::run(&slot, &base, parent, &receiver))
            .ok()?;
        Some(Self { sender: Some(sender), thread: Some(thread) })
    }

    fn run(
        slot: &Mutex<LiveSlot>,
        base: &LiveBase,
        parent: Option<(B256, Box<LiveState>)>,
        receiver: &mpsc::Receiver<Message>,
    ) {
        let mut local = match base.source() {
            Ok(local) => local,
            Err(error) => {
                eprintln!("tiger: live state root failed, the walker answers: {error}");
                LiveSlot::put(slot, None);
                return;
            }
        };
        let warm = parent.is_some();
        let mut stream = base.resume(&local, parent).map(|live| Stream {
            stats: StreamStats { fresh: !warm || live.is_empty(), ..StreamStats::default() },
            live,
            streamed: HashedState::default(),
            originals: Mutex::default(),
            updates: TrieUpdates::default(),
        });
        while let Ok(message) = receiver.recv() {
            match message {
                Message::Transaction(changes) => {
                    // Take every transaction already queued as one batch.
                    let mut batch = changes.hashed();
                    let mut transactions = 1;
                    let mut finish = None;
                    let mut flushed = None;
                    while let Ok(next) = receiver.try_recv() {
                        match next {
                            Message::Transaction(changes) => {
                                transactions += 1;
                                batch.extend(&changes.hashed());
                            }
                            Message::Finish(fin, reply) => {
                                finish = Some((fin, reply));
                                break;
                            }
                            Message::Flush(reply) => {
                                flushed = Some(reply);
                                break;
                            }
                        }
                    }
                    if let Some((fin, reply)) = finish {
                        // The final changes cover the batch: finish at once.
                        Self::reply(slot, base, &mut local, stream, &fin, &reply);
                        return;
                    }
                    if let Ok(s) = stream.as_mut()
                        && let Err(error) = {
                            s.stats.transactions += transactions;
                            s.batch(base, &mut local, &batch)
                        }
                    {
                        stream = Err(error);
                    }
                    if let Some(reply) = flushed {
                        let _ = reply.send(());
                    }
                }
                Message::Flush(reply) => {
                    let _ = reply.send(());
                }
                Message::Finish(fin, reply) => {
                    Self::reply(slot, base, &mut local, stream, &fin, &reply);
                    return;
                }
            }
        }
        LiveSlot::put(slot, None);
    }

    fn reply(
        slot: &Mutex<LiveSlot>,
        base: &LiveBase,
        local: &mut WorkerSource<'_>,
        stream: Result<Stream, TrieError>,
        fin: &HashedState,
        reply: &mpsc::Sender<Option<(B256, TrieUpdates)>>,
    ) {
        match stream.and_then(|s| s.finish(base, local, fin)) {
            Ok((root, updates, live)) => {
                LiveSlot::put(slot, Some((root, live)));
                let _ = reply.send(Some((root, updates)));
            }
            Err(error) => {
                eprintln!("tiger: live state root failed, the walker answers: {error}");
                LiveSlot::put(slot, None);
                let _ = reply.send(None);
            }
        }
    }

    /// Streams one executed transaction's changes (plain keys, its values after it).
    pub fn transaction(&self, changes: BlockChanges) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Message::Transaction(changes));
        }
    }

    /// Waits until every transaction streamed so far is applied (tests use it to make sure
    /// streaming does the work rather than the final changes).
    pub fn flush(&self) {
        let (reply, done) = mpsc::channel();
        if let Some(sender) = &self.sender
            && sender.send(Message::Flush(reply)).is_ok()
        {
            let _ = done.recv();
        }
    }

    /// The root and trie updates after the block's final hashed changes `fin`; `None` when the
    /// session failed and the caller must use the walker.
    pub fn finish(mut self, fin: &HashedState) -> Option<(B256, TrieUpdates)> {
        let (reply, answer) = mpsc::channel();
        self.sender.take()?.send(Message::Finish(fin.clone(), reply)).ok()?;
        let result = answer.recv().ok().flatten();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        result
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Closing the channel ends the thread, which drops the live trie.
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
