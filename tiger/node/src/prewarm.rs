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
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
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

use crate::{BlockExecution, Chain, Node, ParentState, StoredBlock, block_changes};

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

/// Runs `validate` (a block's serial execution and state root on this thread) while
/// [`FULL_PREWARM_WORKERS`] threads warm the datadir pages it will read, as base-reth's prewarm
/// does: each executes every `FULL_PREWARM_WORKERS`-th transaction (after the first, the L1
/// info deposit, which sets what fees read) speculatively over the parent state, then walks the
/// trie paths of what its transactions changed, which the state root reads. Their results are
/// discarded; reading only faults pages into the cache, so a wrong speculation costs nothing
/// but the work. A full state only: a partial state's reads go to the upstream instead.
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
    thread::scope(|scope| {
        for worker in 0..workers {
            let done = &done;
            // A speculation runs over state the block does not see; whatever it hits must not
            // fail the block.
            scope.spawn(move || {
                let block = PrewarmBlock { chain, spec, header, transactions, parent };
                let warm = || block.warm(worker, workers, done);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(warm));
            });
        }
        let result = validate();
        // The workers stop at their next transaction or account.
        done.store(true, Ordering::Relaxed);
        result
    })
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
    /// One prewarm worker: transactions `1 + worker`, `1 + worker + workers`, ….
    fn warm(&self, worker: usize, workers: usize, done: &AtomicBool) {
        let Ok(state) = self.chain.state_after(self.parent) else { return };
        let env = BlockExecution::env(self.header);
        let Ok(mut executor) = BaseBlockExecutor::for_block(self.spec, &env, state.database()) else { return };
        let mine = self.transactions.iter().take(1).chain(self.transactions.iter().skip(1 + worker).step_by(workers));
        for raw in mine {
            if done.load(Ordering::Relaxed) {
                return;
            }
            let Ok((_, tx)) = Transactions::decode(raw) else { continue };
            if let Err(HandlerError::Fatal(_)) = executor.execute_transaction(&tx) {
                break;
            }
        }
        let (_, _, changes) = executor.finish();
        let changes = block_changes(&changes);
        let mut touched: BTreeMap<Address, Vec<U256>> =
            changes.accounts.iter().map(|(address, _)| (*address, Vec::new())).collect();
        for (address, slot, _) in &changes.storage {
            touched.entry(*address).or_default().push(*slot);
        }
        for (address, slots) in touched {
            if done.load(Ordering::Relaxed) {
                return;
            }
            let _ = state.proof(address, &slots);
        }
    }
}
