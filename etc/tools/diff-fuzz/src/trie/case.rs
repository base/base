//! Trie fuzz cases: block sequences of hashed-key writes and proof reads, and the requests and
//! expected results they build into.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_trie::KECCAK_EMPTY;
use serde::{Deserialize, Serialize};

use crate::{
    CorpusCase, ProofTarget, TrieBlock, TrieBlockResult, TrieInput, TrieModel, TrieOp, TrieRequest,
};

/// One block of a [`TrieCase`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseBlock {
    /// Blocks to drop before this one (clamped to the blocks applied so far).
    #[serde(default)]
    pub unwind: u64,
    /// Writes, in order. A `storage` write to an account that does not exist yet gets an
    /// account creation inserted before it when the case is built.
    pub ops: Vec<TrieOp>,
    /// Proof reads after the block.
    #[serde(default)]
    pub proofs: Vec<ProofTarget>,
}

/// Preimages of hashed keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preimages {
    /// Address of each hashed account key.
    pub addresses: BTreeMap<B256, Address>,
    /// Slot index of each hashed storage slot.
    pub slots: BTreeMap<B256, B256>,
}

/// A trie fuzz case: blocks applied from the empty state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieCase {
    /// Blocks in order.
    pub blocks: Vec<CaseBlock>,
    /// Preimages of the case's keys. When every key and slot of the case has one, the request
    /// carries them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preimages: Option<Preimages>,
}

impl CorpusCase for TrieCase {
    fn id(&self) -> String {
        let digest = keccak256(serde_json::to_vec(self).expect("cases serialize"));
        alloy_primitives::hex::encode(&digest[..8])
    }

    /// One plus a unit per 64 writes and proof reads: run time grows with both.
    fn cost(&self) -> f64 {
        let work: usize = self.blocks.iter().map(|b| b.ops.len() + b.proofs.len()).sum();
        1.0 + work as f64 / 64.0
    }
}

/// One block as a request sends it (see `TrieCase::sent`).
struct SentBlock {
    unwind: u64,
    revert: Vec<TrieOp>,
    ops: Vec<TrieOp>,
    /// The state the block's ops start from.
    start: TrieModel,
    /// The state after the block.
    model: TrieModel,
}

/// A built case: the request every adapter gets and what the harness's model expects.
#[derive(Debug, Clone)]
pub struct BuiltCase {
    /// The request.
    pub request: TrieRequest,
    /// The model's result per block.
    pub expected: Vec<TrieBlockResult>,
}

impl TrieCase {
    /// Blocks a request holds at most; later blocks are not sent.
    pub const MAX_BLOCKS: usize = 256;
    /// Writes (`revert` plus `ops`) a block holds at most; later ops are not sent, and an unwind
    /// whose revert is larger is not taken.
    pub const MAX_WRITES: usize = 2048;
    /// Proof targets a block holds at most.
    pub const MAX_PROOFS: usize = 256;
    /// Storage changes (see `storage_changes`) a block holds at most, counting `revert` and
    /// `ops`; later ops are not sent, and an unwind whose revert is larger is not taken.
    pub const MAX_STORAGE_CHANGES: usize = 8192;
    /// Slots a proof target holds at most.
    pub const MAX_PROOF_SLOTS: usize = 64;

    /// The account an implicit creation before a storage write sets.
    pub const fn implicit_account(key: B256) -> TrieOp {
        TrieOp::account(key, 1, U256::ZERO, KECCAK_EMPTY)
    }

    /// The state each block starts from (after its unwind), and the state after the last
    /// block, with the ops each block actually applies (implicit creations included).
    pub fn replay(&self) -> (Vec<TrieModel>, TrieModel) {
        let mut starts = Vec::with_capacity(self.blocks.len());
        let mut states = vec![TrieModel::default()];
        for block in self.blocks.iter().take(Self::MAX_BLOCKS) {
            let sent = Self::sent(&mut states, block);
            starts.push(sent.start);
            states.push(sent.model);
        }
        (starts, states.pop().expect("at least the empty state"))
    }

    /// What a request sends for `block` after the chain states `states`, within the limits:
    /// unwinds `states` and returns the block's writes and the state after it.
    fn sent(states: &mut Vec<TrieModel>, block: &CaseBlock) -> SentBlock {
        let mut unwind = block.unwind.min(states.len() as u64 - 1);
        let mut model = states.last().expect("the empty state stays").clone();
        let mut revert = Vec::new();
        let mut storage = 0;
        if unwind > 0 {
            let target = states[states.len() - 1 - unwind as usize].clone();
            revert = model.diff_to(&target);
            let mut tip = model.clone();
            let revert_storage: usize = revert
                .iter()
                .map(|op| {
                    let changes = Self::storage_changes(&tip, op);
                    tip.apply(op);
                    changes
                })
                .sum();
            if revert.len() > Self::MAX_WRITES || revert_storage > Self::MAX_STORAGE_CHANGES {
                // Too large to revert in one block: the block builds on the tip instead.
                revert.clear();
                unwind = 0;
            } else {
                states.truncate(states.len() - unwind as usize);
                model = target;
                storage = revert_storage;
            }
        }
        let start = model.clone();
        let mut ops = Vec::with_capacity(block.ops.len());
        for op in &block.ops {
            // A storage write to a missing account also creates it.
            let creates =
                matches!(op, TrieOp::Storage { key, .. } if !model.accounts.contains_key(key));
            let changes = Self::storage_changes(&model, op);
            if revert.len() + ops.len() + 1 + usize::from(creates) > Self::MAX_WRITES
                || storage + changes > Self::MAX_STORAGE_CHANGES
            {
                break;
            }
            storage += changes;
            Self::apply_normalized(&mut model, op, &mut ops);
        }
        SentBlock { unwind, revert, ops, start, model }
    }

    /// Storage changes `op` makes on `model`: one per slot write, and every slot a wipe or
    /// delete clears (implementations keyed by hashed slot without a slot listing write each).
    fn storage_changes(model: &TrieModel, op: &TrieOp) -> usize {
        match op {
            TrieOp::Storage { .. } => 1,
            TrieOp::Wipe { key, .. } | TrieOp::Delete { key, .. } => {
                model.accounts.get(key).map_or(0, |account| account.storage.len())
            }
            TrieOp::Account { .. } => 0,
        }
    }

    /// Applies `op` to `model` as a built request does, pushing the ops it becomes to `out`: an
    /// implicit account creation before a storage write to a missing account, and nonce 1 for
    /// an empty account.
    pub fn apply_normalized(model: &mut TrieModel, op: &TrieOp, out: &mut Vec<TrieOp>) {
        if let TrieOp::Storage { key, .. } = op
            && !model.accounts.contains_key(key)
        {
            let create = Self::implicit_account(*key);
            model.apply(&create);
            out.push(create);
        }
        let mut op = op.clone();
        // No empty account can exist since EIP-7523, so none is sent.
        if let TrieOp::Account { nonce, balance, code_hash, .. } = &mut op
            && *nonce == 0
            && balance.is_zero()
            && *code_hash == KECCAK_EMPTY
        {
            *nonce = 1;
        }
        model.apply(&op);
        out.push(op);
    }

    /// Builds the request with id `id` and scratch directory `dir`, and the model's results.
    pub fn build(&self, id: String, dir: String) -> BuiltCase {
        let mut states = vec![TrieModel::default()];
        let mut blocks = Vec::with_capacity(self.blocks.len());
        let mut expected = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.iter().take(Self::MAX_BLOCKS) {
            let SentBlock { unwind, revert, ops, model, .. } = Self::sent(&mut states, block);
            let proofs: Vec<ProofTarget> = block
                .proofs
                .iter()
                .take(Self::MAX_PROOFS)
                .map(|proof| ProofTarget {
                    slots: proof.slots.iter().take(Self::MAX_PROOF_SLOTS).copied().collect(),
                    ..proof.clone()
                })
                .collect();
            let number = states.len() as u64;
            let mut written: Vec<B256> = revert.iter().chain(&ops).map(TrieOp::key).collect();
            written.sort();
            written.dedup();
            expected.push(model.result(number, &written, &proofs));
            states.push(model);
            blocks.push(TrieBlock { number, unwind, revert, ops, proofs });
        }
        if let Some(preimages) = &self.preimages {
            Self::attach(&mut blocks, preimages);
        }
        BuiltCase {
            request: TrieRequest { id, mode: "trie".to_owned(), trie: TrieInput { dir, blocks } },
            expected,
        }
    }

    /// Adds preimages to every op and proof of `blocks` when all of them have one.
    fn attach(blocks: &mut [TrieBlock], preimages: &Preimages) {
        let complete = blocks.iter().all(|block| {
            block.revert.iter().chain(&block.ops).all(|op| match op {
                TrieOp::Storage { key, slot, .. } => {
                    preimages.addresses.contains_key(key) && preimages.slots.contains_key(slot)
                }
                op => preimages.addresses.contains_key(&op.key()),
            }) && block.proofs.iter().all(|p| {
                preimages.addresses.contains_key(&p.key)
                    && p.slots.iter().all(|s| preimages.slots.contains_key(s))
            })
        });
        if !complete {
            return;
        }
        for block in blocks {
            for op in block.revert.iter_mut().chain(&mut block.ops) {
                match op {
                    TrieOp::Account { key, address, .. }
                    | TrieOp::Wipe { key, address }
                    | TrieOp::Delete { key, address } => {
                        *address = Some(preimages.addresses[key]);
                    }
                    TrieOp::Storage { key, slot, address, slot_key, .. } => {
                        *address = Some(preimages.addresses[key]);
                        *slot_key = Some(preimages.slots[slot]);
                    }
                }
            }
            for proof in &mut block.proofs {
                proof.address = Some(preimages.addresses[&proof.key]);
                proof.slot_keys = Some(proof.slots.iter().map(|s| preimages.slots[s]).collect());
            }
        }
    }

    /// Every account key the case names, in first-use order.
    pub fn account_keys(&self) -> Vec<B256> {
        let mut keys = Vec::new();
        for block in &self.blocks {
            let named = block.ops.iter().map(TrieOp::key).chain(block.proofs.iter().map(|p| p.key));
            for key in named {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// Every storage slot the case names under `key`, in first-use order.
    pub fn slots_of(&self, key: &B256) -> Vec<B256> {
        let mut slots = Vec::new();
        for block in &self.blocks {
            let written = block.ops.iter().filter_map(|op| match op {
                TrieOp::Storage { key: k, slot, .. } if k == key => Some(*slot),
                _ => None,
            });
            let read = block.proofs.iter().filter(|p| p.key == *key).flat_map(|p| p.slots.clone());
            for slot in written.chain(read) {
                if !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
        }
        slots
    }

    /// The longest leading-nibble prefix two account keys of the case, or two slots of one
    /// account, share: how deep a key search a real trie would need to reproduce the case.
    pub fn depth(&self) -> usize {
        let shared = |mut keys: Vec<B256>| {
            keys.sort();
            keys.dedup();
            keys.windows(2)
                .map(|pair| U256::from_be_bytes((pair[0] ^ pair[1]).0).leading_zeros() / 4)
                .max()
                .unwrap_or(0)
        };
        let accounts = self.account_keys();
        let slots = accounts.iter().map(|key| shared(self.slots_of(key))).max().unwrap_or(0);
        shared(accounts).max(slots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwinds_revert_to_the_earlier_state_and_number_from_it() {
        let a = B256::repeat_byte(1);
        let b = B256::repeat_byte(2);
        let set = |key, nonce| TrieOp::account(key, nonce, U256::ZERO, KECCAK_EMPTY);
        let case = TrieCase {
            blocks: vec![
                CaseBlock { ops: vec![set(a, 1)], ..Default::default() },
                CaseBlock {
                    ops: vec![TrieOp::storage(b, a, B256::with_last_byte(3))],
                    ..Default::default()
                },
                CaseBlock { unwind: 9, ops: vec![set(a, 2)], ..Default::default() },
            ],
            preimages: None,
        };
        let built = case.build("t".to_owned(), "/tmp/x".to_owned());
        let blocks = &built.request.trie.blocks;
        // The storage write created its account first.
        assert_eq!(blocks[1].ops[0], TrieCase::implicit_account(b));
        // The unwind clamps to the two applied blocks and reverts to the empty state.
        assert_eq!((blocks[2].unwind, blocks[2].number), (2, 1));
        assert_eq!(blocks[2].revert, vec![TrieOp::delete(a), TrieOp::delete(b)]);
        let mut model = TrieModel::default();
        model.apply(&set(a, 2));
        assert_eq!(built.expected[2].state_root, model.result(1, &[], &[]).state_root);
        assert_eq!(built.expected[2].storage_roots.as_ref().unwrap().len(), 1);
        assert_eq!(case.replay().1, model);
    }

    #[test]
    fn blocks_stay_within_the_write_and_storage_change_limits() {
        let a = B256::repeat_byte(1);
        let slot = |i: usize| B256::from(U256::from(i + 1));
        let writes = |from: usize| CaseBlock {
            ops: (from..from + 2048)
                .map(|i| TrieOp::storage(a, slot(i), B256::with_last_byte(1)))
                .collect(),
            ..Default::default()
        };
        let mut blocks: Vec<CaseBlock> = (0..5).map(|i| writes(i * 2048)).collect();
        // The wipe clears more slots than a block may change; the write before it fits.
        blocks.push(CaseBlock {
            ops: vec![TrieOp::storage(a, slot(0), B256::ZERO), TrieOp::wipe(a)],
            ..Default::default()
        });
        // Reverting to the empty state would clear every slot, so the unwind is not taken.
        blocks.push(CaseBlock { unwind: 6, ..Default::default() });
        // Reverting that block and the wipe block restores one slot.
        blocks.push(CaseBlock { unwind: 2, ..Default::default() });
        let case = TrieCase { blocks, preimages: None };
        let built = case.build("t".to_owned(), "/tmp/x".to_owned());
        let sent = &built.request.trie.blocks;
        // The first block's implicit account creation pushes its last write out.
        assert_eq!(sent[0].ops.len(), 2048);
        assert_eq!(
            sent[0].ops.last(),
            Some(&TrieOp::storage(a, slot(2046), B256::with_last_byte(1)))
        );
        assert_eq!(sent[1].ops.len(), 2048);
        assert_eq!(sent[5].ops, vec![TrieOp::storage(a, slot(0), B256::ZERO)]);
        assert_eq!((sent[6].unwind, sent[6].revert.len()), (0, 0));
        assert_eq!(sent[7].unwind, 2);
        assert_eq!(sent[7].revert, vec![TrieOp::storage(a, slot(0), B256::with_last_byte(1))]);
        let (_, last) = case.replay();
        assert_eq!(built.expected[7].state_root, last.result(6, &[], &[]).state_root);
    }
}
