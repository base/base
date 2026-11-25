//! A block's state root computed while the block executes: each transaction's committed changes
//! stream into the live trie ([`crate::LiveState`]) on a background thread, which reveals and
//! hashes them as they come, so little is left when the block ends. The block's own thread
//! finishes it, taking the trie from the background thread (which holds it only while applying
//! one batch), so a busy machine that leaves the background thread behind costs no wait for it.
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
};

use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
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

/// When the live trie holds more nodes than this, its least recently used ones are blinded
/// (revealed again from the stored trie when needed) down to half of it.
/// About 120 bytes each plus its leaf value, so a few hundred megabytes at most.
pub const LIVE_MAX_NODES: usize = 1_000_000;

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
    /// Most nodes the live trie keeps between blocks (see [`LIVE_MAX_NODES`]).
    pub max_nodes: usize,
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

    /// The kept live trie when it holds this base's state (ending the block it computed, which
    /// may prune it, see [`LiveState::end_block`]), else a new one.
    fn resume(
        &self,
        local: &WorkerSource<'_>,
        parent: Option<(B256, Box<LiveState>)>,
    ) -> Result<Box<LiveState>, TrieError> {
        Ok(match parent {
            Some((root, mut live)) if local.view().state_root(false)?.0 == root => {
                live.end_block(self.max_nodes);
                live
            }
            _ => Box::default(),
        })
    }

    /// A new stream over the kept trie `parent` (see [`Self::resume`]).
    fn stream(
        &self,
        local: &WorkerSource<'_>,
        parent: Option<(B256, Box<LiveState>)>,
    ) -> Result<Stream, TrieError> {
        let warm = parent.is_some();
        let live = self.resume(local, parent)?;
        Ok(Stream {
            stats: StreamStats { fresh: !warm || live.is_empty(), ..StreamStats::default() },
            live,
            streamed: HashedState::default(),
            originals: Mutex::default(),
            updates: TrieUpdates::default(),
        })
    }

    /// Whether `TIGER_LIVE_STATS=1` asks for a line per streamed block.
    pub fn stats_enabled() -> bool {
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

    /// Returns the live trie to `slot` after a root: kept at `root` when it succeeded (the
    /// next root that resumes it prunes it, off this block's path).
    pub fn put(slot: &Mutex<Self>, kept: Option<(B256, Box<LiveState>)>) {
        *slot.lock().expect("unpoisoned") = match kept {
            Some((root, live)) => Self::Idle(root, live),
            None => Self::Empty,
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
    /// Answered once everything sent before it is applied.
    Flush(mpsc::Sender<()>),
}

/// Where a session's trie is, shared by its background thread and its owner.
enum Progress {
    /// Not started yet: the kept trie to resume.
    Waiting(Option<(B256, Box<LiveState>)>),
    /// Applying streamed transactions.
    Streaming(Stream),
    /// Streaming failed; the walker answers.
    Failed(TrieError),
    /// The owner took it to finish the block, or dropped the session.
    Taken,
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
    /// Slots only the final changes name.
    unseen: usize,
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
        // A storage trie keeps its streamed state, which the final changes then apply over: a
        // slot only they name (written outside transactions, such as by a system call) just
        // applies, and a slot the stream changed that they leave out ended at its parent value
        // (they drop such slots), so it goes back to that value. A streamed wipe they do not
        // have is kept when the parent storage is empty (the block's accumulator drops the wipe
        // of a created account). Other wipe differences and deleted accounts drop the trie,
        // which is recomputed from the stored trie.
        let started = std::time::Instant::now();
        let requested = fin;
        let mut fin = std::borrow::Cow::Borrowed(fin);
        let mut kept_wipes = BTreeSet::new();
        let mut restore: BTreeSet<B256> = BTreeSet::new();
        for (address, streamed) in &self.streamed.storages {
            let wanted = fin.storages.get(address);
            let wiped = wanted.is_some_and(|f| f.wiped);
            let deleted = fin.accounts.get(address).is_some_and(Option::is_none);
            let reachable = !deleted
                && (wiped == streamed.wiped
                    || (streamed.wiped
                        && !wiped
                        && local.view().storage_root(*address)? == EMPTY_ROOT_HASH));
            if let Some(f) = wanted {
                self.stats.unseen +=
                    f.slots.keys().filter(|k| !streamed.slots.contains_key(*k)).count();
            }
            if !reachable {
                self.stats.dropped += 1;
                self.live.storages.remove(address);
                self.updates.storage_tries.remove(address);
                restore.insert(*address);
                continue;
            }
            if wiped {
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
                "tiger: live root fresh={} txs={} batches={} accounts={} storages={} dropped={} reverted={} unseen={} finish_us={} nodes={}",
                stats.fresh,
                stats.transactions,
                stats.batches,
                requested.accounts.len(),
                requested.storages.len(),
                stats.dropped,
                stats.reverted,
                stats.unseen,
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
pub struct LiveSession {
    sender: Option<mpsc::Sender<Message>>,
    progress: Arc<Mutex<Progress>>,
    slot: Arc<Mutex<LiveSlot>>,
    base: LiveBase,
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSession").finish_non_exhaustive()
    }
}

impl LiveSession {
    /// Starts a session over `base` with the live trie in `slot`; `None` when it is in use.
    pub fn start(slot: Arc<Mutex<LiveSlot>>, base: LiveBase) -> Option<Self> {
        let parent = LiveSlot::take(&slot)?;
        let progress = Arc::new(Mutex::new(Progress::Waiting(parent)));
        let (sender, receiver) = mpsc::channel::<Message>();
        let (shared, thread_base) = (Arc::clone(&progress), base.clone());
        let spawned = std::thread::Builder::new()
            .name("live-root".into())
            .spawn(move || Self::run(&thread_base, &shared, &receiver));
        // Without the thread, the owner computes everything when it finishes.
        let sender = spawned.is_ok().then_some(sender);
        Some(Self { sender, progress, slot, base })
    }

    /// The background thread: applies queued transactions in batches until the owner takes
    /// the trie or the channel closes. It holds the trie only while applying one batch, so
    /// finishing waits for at most that.
    fn run(base: &LiveBase, progress: &Mutex<Progress>, receiver: &mpsc::Receiver<Message>) {
        let mut local = None;
        while let Ok(message) = receiver.recv() {
            let mut batch = HashedState::default();
            let mut transactions = 0;
            let mut flushed = Vec::new();
            let mut next = Some(message);
            while let Some(message) = next {
                match message {
                    Message::Transaction(changes) => {
                        transactions += 1;
                        batch.extend(&changes.hashed());
                    }
                    Message::Flush(reply) => flushed.push(reply),
                }
                next = receiver.try_recv().ok();
            }
            if transactions > 0 {
                let mut progress = progress.lock().expect("unpoisoned");
                let state = std::mem::replace(&mut *progress, Progress::Taken);
                *progress = match state {
                    Progress::Taken => return,
                    Progress::Failed(error) => Progress::Failed(error),
                    Progress::Waiting(parent) => {
                        match Self::apply(base, &mut local, None, parent, &batch, transactions) {
                            Ok(stream) => Progress::Streaming(stream),
                            Err(error) => Progress::Failed(error),
                        }
                    }
                    Progress::Streaming(stream) => {
                        match Self::apply(
                            base,
                            &mut local,
                            Some(stream),
                            None,
                            &batch,
                            transactions,
                        ) {
                            Ok(stream) => Progress::Streaming(stream),
                            Err(error) => Progress::Failed(error),
                        }
                    }
                };
            }
            for reply in flushed {
                let _ = reply.send(());
            }
        }
    }

    /// Applies `batch` to `stream`, or to a new stream over the kept trie `parent`, opening
    /// the thread's source on first use.
    fn apply<'b>(
        base: &'b LiveBase,
        local: &mut Option<WorkerSource<'b>>,
        stream: Option<Stream>,
        parent: Option<(B256, Box<LiveState>)>,
        batch: &HashedState,
        transactions: usize,
    ) -> Result<Stream, TrieError> {
        let source = match local {
            Some(source) => source,
            None => local.insert(base.source()?),
        };
        let mut stream = match stream {
            Some(stream) => stream,
            None => base.stream(source, parent)?,
        };
        stream.stats.transactions += transactions;
        stream.batch(base, source, batch)?;
        Ok(stream)
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

    /// The root and trie updates after the block's final hashed changes `fin`, computed on
    /// this thread from what the background thread applied (transactions it has not reached
    /// are covered by `fin`); `None` when the session failed and the caller must use the
    /// walker.
    pub fn finish(mut self, fin: &HashedState) -> Option<(B256, TrieUpdates)> {
        self.sender.take();
        let state =
            std::mem::replace(&mut *self.progress.lock().expect("unpoisoned"), Progress::Taken);
        let result = self.base.source().and_then(|mut local| {
            let stream = match state {
                Progress::Waiting(parent) => self.base.stream(&local, parent)?,
                Progress::Streaming(stream) => stream,
                Progress::Failed(error) => return Err(error),
                Progress::Taken => unreachable!("only the owner takes the trie"),
            };
            stream.finish(&self.base, &mut local, fin)
        });
        match result {
            Ok((root, updates, live)) => {
                LiveSlot::put(&self.slot, Some((root, live)));
                Some((root, updates))
            }
            Err(error) => {
                eprintln!("tiger: live state root failed, the walker answers: {error}");
                LiveSlot::put(&self.slot, None);
                None
            }
        }
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Unfinished (the block failed): the trie holds no state a later root can use.
        self.sender.take();
        let state =
            std::mem::replace(&mut *self.progress.lock().expect("unpoisoned"), Progress::Taken);
        if !matches!(state, Progress::Taken) {
            LiveSlot::put(&self.slot, None);
        }
    }
}
