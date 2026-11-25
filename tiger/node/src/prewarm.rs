//! Prewarming a stateless node's partial state: for blocks waiting to be imported, several at a
//! time, the upstream's `prestateTracer` names the accounts and slots each block reads, and the
//! ones the stored tip's partial state does not hold are fetched as proofs at the tip (and their
//! code) and stored where they link by hash to the tip's state root. The import then finds most
//! of its state local and needs few proof rounds of its own, which run one block at a time. The
//! traced keys are only a hint (a reorged block reads others), so a wrong one only costs a fetch.
//! The keys the block deletes are prewarmed too: the trie nodes their deletions collapse into
//! (see [`Node::collapse_siblings`]), and the block's whole witness when one is a branch.
//!
//! A full state prewarms each block in process while it executes instead ([`warm_while`]).

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex, mpsc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256};
use base_tiger_evm::{BaseBlockExecutor, ChainSpec, Transactions};
use evm2::{
    evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS},
    registry::HandlerError,
};

use base_tiger_trie::BlockChanges;

use crate::{BlockExecution, Chain, Node, ParentState, StoredBlock, TransactionChanges, block_changes};

/// Blocks prewarmed at once.
const PREWARM_WORKERS: usize = 12;
/// Rounds of reading and fetching per prewarmed block: accounts and slots, then the code of the
/// accounts the first round proved.
const PREWARM_ROUNDS: usize = 2;
/// The ring size of the EIP-2935 block-hash and EIP-4788 beacon-root contracts.
const HISTORY_WINDOW: u64 = 8191;
/// How far above the head blocks are prewarmed: farther ones (such as gossiped payloads while a
/// long backfill runs) wait, so the blocks imported next are prewarmed first.
const PREWARM_AHEAD: u64 = 512;
/// Most blocks waiting; the highest are dropped beyond it.
const MAX_QUEUED: usize = 4096;
/// How often an idle worker looks again at the head.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// The prewarm queue: blocks by number (with timestamp and hash), lowest first.
#[derive(Debug, Default)]
pub struct Prewarm {
    queue: Mutex<BTreeMap<u64, (u64, B256)>>,
    queued: Condvar,
}

impl Node {
    /// Starts the prewarm workers (a stateless node only).
    pub fn start_prewarm(self: &Arc<Self>) {
        if self.upstream.is_none() {
            return;
        }
        assert!(self.prewarm.set(Prewarm::default()).is_ok(), "prewarm starts once");
        for _ in 0..PREWARM_WORKERS {
            let node = Arc::clone(self);
            thread::spawn(move || {
                loop {
                    let (number, timestamp, hash) = node.next_prewarm();
                    node.prewarm_block(number, timestamp, hash);
                }
            });
        }
    }

    /// Queues block `header` (hash `hash`) for prewarming (no-op unless prewarming runs).
    pub fn prewarm(&self, header: &Header, hash: B256) {
        let Some(prewarm) = self.prewarm.get() else { return };
        let mut queue = prewarm.queue.lock().expect("prewarm queue");
        queue.entry(header.number).or_insert((header.timestamp, hash));
        while queue.len() > MAX_QUEUED {
            queue.pop_last();
        }
        prewarm.queued.notify_one();
    }

    /// The lowest queued block above the head and within [`PREWARM_AHEAD`] of it, waiting for
    /// one; blocks at or below the head are dropped.
    fn next_prewarm(&self) -> (u64, u64, B256) {
        let prewarm = self.prewarm.get().expect("prewarm started");
        loop {
            let head = self.chain.read().expect("chain lock").head().number();
            let mut queue = prewarm.queue.lock().expect("prewarm queue");
            while queue.first_key_value().is_some_and(|(number, _)| *number <= head) {
                queue.pop_first();
            }
            if let Some(entry) = queue.first_entry()
                && *entry.key() <= head + PREWARM_AHEAD
            {
                let (number, (timestamp, hash)) = entry.remove_entry();
                return (number, timestamp, hash);
            }
            let _ = prewarm.queued.wait_timeout(queue, IDLE_POLL).expect("prewarm queue");
        }
    }

    fn prewarm_block(&self, number: u64, timestamp: u64, hash: B256) {
        let Some(upstream) = &self.upstream else { return };
        let Ok(mut keys) = upstream.block_keys(number) else { return };
        // The pre-execution system calls (EIP-2935, EIP-4788) write slots no transaction trace
        // shows.
        let window = U256::from(HISTORY_WINDOW);
        keys.read
            .entry(HISTORY_STORAGE_ADDRESS)
            .or_default()
            .insert(U256::from(number.saturating_sub(1)) % window);
        let beacon = U256::from(timestamp) % window;
        keys.read.entry(BEACON_ROOTS_ADDRESS).or_default().extend([beacon, beacon + window]);
        for _ in 0..PREWARM_ROUNDS {
            let (misses, tip) = {
                let chain = self.chain.read().expect("chain lock");
                if !chain.db.is_partial() || number <= chain.head().number() {
                    return;
                }
                let head = chain.head();
                let Ok(state) = chain.state_after(&head) else { return };
                let state = state.collecting();
                for (address, slots) in &keys.read {
                    if let Ok(Some(account)) = state.account(address)
                        && account.code_hash != KECCAK256_EMPTY
                        && !account.code_hash.is_zero()
                    {
                        let _ = state.code(&account.code_hash);
                    }
                    for slot in slots {
                        let _ = state.storage(address, slot);
                    }
                }
                (state.take_misses(), head.hash)
            };
            if misses.is_empty() {
                break;
            }
            let Ok(proofs) = self.fetch_proofs(tip, &misses) else { return };
            let chain = self.chain.read().expect("chain lock");
            if Self::absorb(&chain, &proofs).is_err() {
                return;
            }
        }
        if keys.deleted.is_empty() {
            return;
        }
        let tip = self.chain.read().expect("chain lock").head().hash;
        let Ok((proofs, branch)) = self.collapse_siblings(tip, number, &keys.deleted) else {
            return;
        };
        // A deletion collapsing into a branch needs the branch's node, which only the witness has.
        if branch && let Some(witnesses) = &self.witnesses {
            witnesses.request(hash, number);
        }
        let chain = self.chain.read().expect("chain lock");
        let _ = Self::absorb(&chain, &proofs);
    }
}

/// Threads that prewarm a block on a full state while it executes.
const FULL_PREWARM_WORKERS: usize = 8;
/// Threads that walk the trie paths of what the prewarm workers' transactions change.
const FULL_PREWARM_WALKERS: usize = 4;
/// Most storage slots of one account a walker walks at once, so the walkers share a storage
/// trie that a block changes in many places.
const WALK_SLOTS: usize = 16;
/// How long an idle walker waits for changes before it looks for paths to walk again.
const WALK_POLL: Duration = Duration::from_millis(1);
/// Threads that run ahead of the block's transactions (see [`PrewarmBlock::run_ahead`]).
const RUN_AHEAD_THREADS: usize = 2;
/// Threads that read the slots the run-ahead threads miss, so they are read concurrently.
const FETCHERS: usize = 8;
/// Most passes over one transaction: each reaches one read deeper into its chains of
/// dependent storage reads.
const RUN_AHEAD_PASSES: usize = 32;
/// Longest a run-ahead pass waits for the slots it missed before the next pass.
const PASS_WAIT: Duration = Duration::from_millis(2);

/// Whether prewarm runs ahead of the block's transactions (opt-in: `TIGER_RUN_AHEAD=1`).
fn run_ahead_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TIGER_RUN_AHEAD").is_ok_and(|v| v == "1"))
}

/// The slots the run-ahead threads asked for: read (with their values) or still being read.
#[derive(Debug, Default)]
pub(crate) struct Fetches {
    slots: Mutex<HashMap<(Address, U256), Option<U256>>>,
    landed: Condvar,
}

impl Fetches {
    /// The value of `slot` of `address` if a fetcher has read it; otherwise queues it on `fetch`
    /// (once) and returns `None`.
    pub(crate) fn read(&self, address: Address, slot: U256, fetch: &mpsc::Sender<(Address, U256)>) -> Option<U256> {
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match slots.get(&(address, slot)) {
            Some(value) => *value,
            None => {
                slots.insert((address, slot), None);
                let _ = fetch.send((address, slot));
                None
            }
        }
    }

    fn land(&self, address: Address, slot: U256, value: U256) {
        self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert((address, slot), Some(value));
        self.landed.notify_all();
    }

    /// Waits up to [`PASS_WAIT`] for every slot asked for so far to be read.
    fn wait_landed(&self) {
        let slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = self.landed.wait_timeout_while(slots, PASS_WAIT, |slots| slots.values().any(Option::is_none));
    }
}

/// Runs `validate` (a block's serial execution and state root on this thread) while threads
/// warm the datadir pages it will read, as base-reth's prewarm does. [`FULL_PREWARM_WORKERS`]
/// workers each execute every `FULL_PREWARM_WORKERS`-th transaction (after the first, the L1
/// info deposit, which sets what fees read) speculatively over the parent state, and send what
/// each transaction changed to [`FULL_PREWARM_WALKERS`] walkers as soon as it commits. The
/// walkers walk the trie paths of those changes, which the state root reads, at most
/// [`WALK_SLOTS`] slots of one account at a time so a storage the block changes in many
/// places is walked by all of them. With `TIGER_RUN_AHEAD=1`, [`RUN_AHEAD_THREADS`] threads
/// also run ahead of each transaction with [`FETCHERS`] threads reading the slots they reach
/// concurrently (see [`PrewarmBlock::run_ahead`]). Their results are discarded; reading only faults pages
/// into the cache, so a wrong speculation costs nothing but the work. A full state only: a
/// partial state's reads go to the upstream instead.
pub(crate) fn warm_while<R>(
    chain: &Chain,
    spec: &ChainSpec,
    header: &Header,
    transactions: &[Bytes],
    parent: &StoredBlock,
    validate: impl FnOnce() -> R,
) -> R {
    let workers = FULL_PREWARM_WORKERS.min(transactions.len().saturating_sub(1));
    if workers == 0 {
        return validate();
    }
    let done = AtomicBool::new(false);
    let (changed, receiver) = mpsc::channel();
    let receiver = Mutex::new(receiver);
    let paths = Mutex::new(Paths::default());
    let fetches = Arc::new(Fetches::default());
    let (fetch, fetched) = mpsc::channel();
    let fetched = Mutex::new(fetched);
    let next = AtomicUsize::new(1);
    thread::scope(|scope| {
        for worker in 0..workers {
            let (done, changed) = (&done, changed.clone());
            // A speculation runs over state the block does not see; whatever it hits must not
            // fail the block.
            scope.spawn(move || {
                let block = PrewarmBlock { chain, spec, header, transactions, parent };
                let warm = || block.warm(worker, workers, &changed, done);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(warm));
            });
        }
        drop(changed);
        for _ in 0..FULL_PREWARM_WALKERS {
            let (done, receiver, paths) = (&done, &receiver, &paths);
            scope.spawn(move || {
                let walk = || walk_paths(chain, parent, receiver, paths, done);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(walk));
            });
        }
        for _ in 0..if run_ahead_enabled() { RUN_AHEAD_THREADS } else { 0 } {
            let (done, next, fetches, fetch) = (&done, &next, Arc::clone(&fetches), fetch.clone());
            scope.spawn(move || {
                let block = PrewarmBlock { chain, spec, header, transactions, parent };
                let run = || block.run_ahead(next, fetches, fetch, done);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
            });
        }
        drop(fetch);
        for _ in 0..if run_ahead_enabled() { FETCHERS } else { 0 } {
            let (done, fetched, fetches) = (&done, &fetched, &fetches);
            scope.spawn(move || {
                let fetch = || fetch_slots(chain, parent, fetched, fetches, done);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(fetch));
            });
        }
        let result = validate();
        // The workers stop at their next transaction, the walkers at their next path.
        done.store(true, Ordering::Relaxed);
        result
    })
}

/// One walker: walks the trie paths of the changes the workers send until they are all walked
/// and the workers are done, or the block is.
fn walk_paths(
    chain: &Chain,
    parent: &StoredBlock,
    receiver: &Mutex<mpsc::Receiver<BlockChanges>>,
    paths: &Mutex<Paths>,
    done: &AtomicBool,
) {
    let Ok(state) = chain.state_after(parent) else { return };
    let paths = || paths.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    while !done.load(Ordering::Relaxed) {
        let next = paths().queued.pop_front();
        if let Some((address, slots)) = next {
            let _ = state.proof(address, &slots);
            continue;
        }
        match receiver.lock().unwrap_or_else(std::sync::PoisonError::into_inner).recv_timeout(WALK_POLL) {
            Ok(changes) => paths().add(&changes),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if paths().queued.is_empty() {
                    return;
                }
            }
        }
    }
}

/// One fetcher: reads the slots the run-ahead threads miss (filling the execution cache) and
/// hands them their values, until the run-ahead threads are done or the block is.
fn fetch_slots(
    chain: &Chain,
    parent: &StoredBlock,
    fetched: &Mutex<mpsc::Receiver<(Address, U256)>>,
    fetches: &Fetches,
    done: &AtomicBool,
) {
    let Ok(state) = chain.state_after(parent) else { return };
    while !done.load(Ordering::Relaxed) {
        let next = fetched.lock().unwrap_or_else(std::sync::PoisonError::into_inner).recv_timeout(WALK_POLL);
        match next {
            Ok((address, slot)) => fetches.land(address, slot, state.storage(&address, &slot).unwrap_or_default()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The trie paths the walkers walk: the accounts and slots queued so far, and the paths still to
/// walk, by account, each with at most [`WALK_SLOTS`] of its slots (none: the account's path
/// only).
#[derive(Debug, Default)]
struct Paths {
    accounts: HashSet<Address>,
    slots: HashSet<(Address, U256)>,
    queued: VecDeque<(Address, Vec<U256>)>,
}

impl Paths {
    /// Queues the paths of `changes` not queued before.
    fn add(&mut self, changes: &BlockChanges) {
        let mut touched: BTreeMap<Address, Vec<U256>> = BTreeMap::new();
        for (address, _) in &changes.accounts {
            if self.accounts.insert(*address) {
                touched.entry(*address).or_default();
            }
        }
        for (address, slot, _) in &changes.storage {
            if self.slots.insert((*address, *slot)) {
                touched.entry(*address).or_default().push(*slot);
            }
        }
        for (address, slots) in touched {
            if slots.len() <= WALK_SLOTS {
                self.queued.push_back((address, slots));
            } else {
                self.queued.extend(slots.chunks(WALK_SLOTS).map(|slots| (address, slots.to_vec())));
            }
        }
    }
}

/// The block a full-state prewarm worker speculates on.
struct PrewarmBlock<'a> {
    chain: &'a Chain,
    spec: &'a ChainSpec,
    header: &'a Header,
    transactions: &'a [Bytes],
    parent: &'a StoredBlock,
}

impl PrewarmBlock<'_> {
    /// One prewarm worker: transactions `1 + worker`, `1 + worker + workers`, …, each sent to
    /// the walkers as it commits.
    fn warm(&self, worker: usize, workers: usize, changed: &mpsc::Sender<BlockChanges>, done: &AtomicBool) {
        let Ok(state) = self.chain.state_after(self.parent) else { return };
        let env = BlockExecution::env(self.header);
        let Ok(mut executor) = BaseBlockExecutor::for_block(self.spec, &env, state.database()) else { return };
        let mine = self.transactions.iter().take(1).chain(self.transactions.iter().skip(1 + worker).step_by(workers));
        for (index, raw) in mine.enumerate() {
            if done.load(Ordering::Relaxed) {
                return;
            }
            let Ok((_, tx)) = Transactions::decode(raw) else { continue };
            let mut changes = TransactionChanges::default();
            let executed = executor.execute_transaction_observed(&tx, &mut changes);
            // Every worker runs the first transaction; the first worker's changes are walked.
            if index > 0 || worker == 0 {
                let _ = changed.send(changes.0);
            }
            if let Err(HandlerError::Fatal(_)) = executed {
                break;
            }
        }
        // The pre-execution system calls' changes, which no transaction reports.
        if worker == 0 {
            let (_, _, changes) = executor.finish();
            let _ = changed.send(block_changes(&changes));
        }
    }

    /// One run-ahead thread: takes the block's transactions in order and executes each (after
    /// the first) over the parent state without waiting on the datadir: slots neither the
    /// execution cache nor the fetchers hold read as zero and go to the fetchers. A transaction
    /// whose reads form chains (a slot's address depends on a value read before it) reaches one
    /// read deeper each pass, so it is executed again once the slots it missed land, up to
    /// [`RUN_AHEAD_PASSES`] times, until a pass misses nothing. A heavy transaction's slots are
    /// read concurrently instead of one after another; the validating thread then finds them
    /// cached.
    fn run_ahead(&self, next: &AtomicUsize, fetches: Arc<Fetches>, fetch: mpsc::Sender<(Address, U256)>, done: &AtomicBool) {
        let Ok(state) = self.chain.state_after(self.parent) else { return };
        let Some(state) = state.running_ahead(fetches.clone(), fetch) else { return };
        let env = BlockExecution::env(self.header);
        let Some(Ok((_, first))) = self.transactions.first().map(|raw| Transactions::decode(raw)) else { return };
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(raw) = self.transactions.get(index) else { return };
            let Ok((_, tx)) = Transactions::decode(raw) else { continue };
            for _ in 0..RUN_AHEAD_PASSES {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                let missed = state.ahead_misses();
                let Ok(mut executor) = BaseBlockExecutor::for_block(self.spec, &env, state.database()) else { return };
                let _ = executor.execute_transaction(&first);
                let _ = executor.execute_transaction(&tx);
                if state.ahead_misses() == missed {
                    break;
                }
                fetches.wait_landed();
            }
        }
    }
}
