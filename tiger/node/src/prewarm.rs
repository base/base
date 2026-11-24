//! Prewarming a stateless node's partial state: for blocks waiting to be imported, several at a
//! time, the upstream's `prestateTracer` names the accounts and slots each block reads, and the
//! ones the stored tip's partial state does not hold are fetched as proofs at the tip (and their
//! code) and stored where they link by hash to the tip's state root. The import then finds most
//! of its state local and needs few proof rounds of its own, which run one block at a time. The
//! traced keys are only a hint (a reorged block reads others), so a wrong one only costs a fetch.
//! The keys the block deletes are prewarmed too: the trie nodes their deletions collapse into
//! (see [`Node::collapse_siblings`]), and the block's whole witness when one is a branch.

use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use alloy_consensus::Header;
use alloy_primitives::{B256, KECCAK256_EMPTY, U256};
use evm2::evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS};

use crate::Node;

/// Blocks prewarmed at once.
const PREWARM_WORKERS: usize = 12;
/// Rounds of reading and fetching per prewarmed block: accounts and slots, then the code of the
/// accounts the first round proved.
const PREWARM_ROUNDS: usize = 2;
/// The ring size of the EIP-2935 block-hash and EIP-4788 beacon-root contracts.
const HISTORY_WINDOW: u64 = 8191;
/// Recently queued block numbers remembered, so one is queued once.
const PREWARM_SEEN: usize = 2048;

/// The prewarm queue.
#[derive(Debug)]
pub struct Prewarm {
    jobs: Mutex<mpsc::Sender<(u64, u64, B256)>>,
    seen: Mutex<(HashSet<u64>, VecDeque<u64>)>,
}

impl Node {
    /// Starts the prewarm workers (a stateless node only).
    pub fn start_prewarm(self: &Arc<Self>) {
        if self.upstream.is_none() {
            return;
        }
        let (sender, receiver) = mpsc::channel::<(u64, u64, B256)>();
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..PREWARM_WORKERS {
            let (node, receiver) = (Arc::clone(self), Arc::clone(&receiver));
            thread::spawn(move || {
                loop {
                    let job = receiver.lock().expect("prewarm jobs").recv();
                    let Ok((number, timestamp, hash)) = job else { return };
                    node.prewarm_block(number, timestamp, hash);
                }
            });
        }
        let prewarm = Prewarm {
            jobs: Mutex::new(sender),
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
        };
        assert!(self.prewarm.set(prewarm).is_ok(), "prewarm starts once");
    }

    /// Queues block `header` (hash `hash`) for prewarming (no-op unless prewarming runs).
    pub fn prewarm(&self, header: &Header, hash: B256) {
        let number = header.number;
        let Some(prewarm) = self.prewarm.get() else { return };
        let mut seen = prewarm.seen.lock().expect("prewarm seen");
        if !seen.0.insert(number) {
            return;
        }
        seen.1.push_back(number);
        while seen.1.len() > PREWARM_SEEN {
            let oldest = seen.1.pop_front().expect("over capacity");
            seen.0.remove(&oldest);
        }
        drop(seen);
        let _ = prewarm.jobs.lock().expect("prewarm jobs").send((number, header.timestamp, hash));
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
