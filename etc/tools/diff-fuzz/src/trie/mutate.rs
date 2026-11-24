//! Structure-aware mutations of trie cases: blocks, ops, values, keys moved to share prefixes
//! with other keys, proof reads, unwinds, and splices.

use alloy_primitives::{B256, U256};

use crate::{CaseBlock, Rng, TrieCase, TrieGen, TrieModel, TrieOp};

/// Mutates trie cases.
#[derive(Debug, Clone, Copy)]
pub struct TrieMutator;

impl TrieMutator {
    /// Mutation names, in the order [`Self::mutate`] picks them by weight.
    pub const NAMES: [&str; 16] = [
        "rerollBlock",
        "insertBlock",
        "removeBlock",
        "duplicateBlock",
        "swapBlocks",
        "insertOps",
        "removeOps",
        "duplicateOp",
        "swapOps",
        "value",
        "rekey",
        "proofs",
        "unwind",
        "splice",
        "emptyOut",
        "truncate",
    ];
    const WEIGHTS: [u64; 16] = [6, 5, 3, 2, 2, 10, 6, 3, 3, 8, 14, 6, 3, 4, 3, 2];
    /// Mutated cases are cut back to this many blocks, so inserts and splices do not grow them
    /// without bound.
    pub const MAX_BLOCKS: usize = 40;

    /// Applies one to three mutations to `case` and returns their names. `donor` is another
    /// corpus case for splicing.
    pub fn mutate(
        rng: &mut Rng,
        case: &mut TrieCase,
        donor: Option<&TrieCase>,
    ) -> Vec<&'static str> {
        let mut applied = Vec::new();
        for _ in 0..rng.range(1, 3) {
            let choice = rng.weighted(&Self::WEIGHTS);
            if Self::apply(rng, case, donor, choice) {
                applied.push(Self::NAMES[choice]);
            }
        }
        case.blocks.truncate(Self::MAX_BLOCKS);
        if case.blocks.is_empty() {
            let mut generator = TrieGen::for_case(rng.fork(1), case);
            case.blocks.push(generator.block(&mut TrieModel::default()));
            case.preimages = generator.preimages;
        }
        applied
    }

    fn block_index(rng: &mut Rng, case: &TrieCase) -> Option<usize> {
        (!case.blocks.is_empty()).then(|| rng.index(case.blocks.len()))
    }

    /// The state block `index` starts from.
    fn start_of(case: &TrieCase, index: usize) -> TrieModel {
        let prefix = TrieCase { blocks: case.blocks[..index].to_vec(), preimages: None };
        let (_, end) = prefix.replay();
        let unwind = case.blocks.get(index).map_or(0, |b| b.unwind);
        if unwind == 0 {
            return end;
        }
        let mut probe = prefix;
        probe.blocks.push(CaseBlock { unwind, ..Default::default() });
        probe.replay().0.pop().unwrap_or_default()
    }

    fn apply(rng: &mut Rng, case: &mut TrieCase, donor: Option<&TrieCase>, choice: usize) -> bool {
        let mut generator = TrieGen::for_case(rng.fork(choice as u64), case);
        let changed = Self::apply_with(rng, &mut generator, case, donor, choice);
        if generator.preimages.is_some() {
            case.preimages = generator.preimages;
        }
        changed
    }

    fn apply_with(
        rng: &mut Rng,
        generator: &mut TrieGen,
        case: &mut TrieCase,
        donor: Option<&TrieCase>,
        choice: usize,
    ) -> bool {
        let len = case.blocks.len();
        match Self::NAMES[choice] {
            "rerollBlock" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let mut model = Self::start_of(case, i);
                let unwind = case.blocks[i].unwind;
                case.blocks[i] = generator.block(&mut model);
                case.blocks[i].unwind = unwind;
            }
            "insertBlock" => {
                let i = rng.index(len + 1);
                let mut model = Self::start_of(case, i);
                case.blocks.insert(i, generator.block(&mut model));
            }
            "removeBlock" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                case.blocks.remove(i);
            }
            "duplicateBlock" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let block = case.blocks[i].clone();
                case.blocks.insert(i + 1, block);
            }
            "swapBlocks" => {
                if len < 2 {
                    return false;
                }
                let i = rng.index(len - 1);
                case.blocks.swap(i, i + 1);
            }
            "insertOps" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let at = rng.index(case.blocks[i].ops.len() + 1);
                let mut model = Self::start_of(case, i);
                for op in &case.blocks[i].ops[..at] {
                    TrieCase::apply_normalized(&mut model, op, &mut Vec::new());
                }
                let mut ops = Vec::new();
                for _ in 0..rng.range(1, 4) {
                    generator.ops(&mut model, &mut ops);
                }
                case.blocks[i].ops.splice(at..at, ops);
            }
            "removeOps" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let ops = &mut case.blocks[i].ops;
                if ops.is_empty() {
                    return false;
                }
                let start = rng.index(ops.len());
                let end = (start + rng.range(1, 8) as usize).min(ops.len());
                ops.drain(start..end);
            }
            "duplicateOp" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let ops = &mut case.blocks[i].ops;
                if ops.is_empty() {
                    return false;
                }
                let op = ops[rng.index(ops.len())].clone();
                let at = rng.index(ops.len() + 1);
                ops.insert(at, op);
            }
            "swapOps" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let ops = &mut case.blocks[i].ops;
                if ops.len() < 2 {
                    return false;
                }
                let (a, b) = (rng.index(ops.len()), rng.index(ops.len()));
                ops.swap(a, b);
            }
            "value" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let ops = &mut case.blocks[i].ops;
                if ops.is_empty() {
                    return false;
                }
                let at = rng.index(ops.len());
                match &mut ops[at] {
                    TrieOp::Storage { value, .. } => {
                        *value = match rng.below(4) {
                            0 => B256::ZERO,
                            1 => {
                                let mut v = U256::from_be_bytes(value.0);
                                v ^= U256::from(1) << rng.below(256);
                                B256::from(v.to_be_bytes::<32>())
                            }
                            _ => generator.value(),
                        };
                    }
                    TrieOp::Account { nonce, balance, code_hash, code, .. } => match rng.below(3) {
                        0 => *nonce = nonce.wrapping_add(1),
                        1 => *balance = generator.rng.u256() >> rng.below(256),
                        _ => (*code_hash, *code) = generator.code(),
                    },
                    op @ (TrieOp::Wipe { .. } | TrieOp::Delete { .. }) => {
                        *op = generator.account(op.key());
                    }
                }
            }
            "rekey" => return Self::rekey(rng, generator, case),
            "proofs" => {
                let Some(i) = Self::block_index(rng, case) else { return false };
                let mut model = Self::start_of(case, i);
                for op in &case.blocks[i].ops {
                    TrieCase::apply_normalized(&mut model, op, &mut Vec::new());
                }
                let proofs = &mut case.blocks[i].proofs;
                if !proofs.is_empty() && rng.chance(30) {
                    proofs.remove(rng.index(proofs.len()));
                } else {
                    proofs.extend(generator.proofs(&model));
                }
            }
            "unwind" => {
                if len < 2 {
                    return false;
                }
                let i = rng.range(1, len as u64 - 1) as usize;
                let block = &mut case.blocks[i];
                block.unwind = if block.unwind > 0 { 0 } else { rng.range(1, (i as u64).min(4)) };
            }
            "splice" => {
                // A case with preimages takes blocks only from donors with preimages, so every
                // key keeps one.
                let Some(donor) = donor.filter(|d| {
                    !d.blocks.is_empty() && (generator.preimages.is_none() || d.preimages.is_some())
                }) else {
                    return false;
                };
                let start = rng.index(donor.blocks.len());
                let end = (start + rng.range(1, 4) as usize).min(donor.blocks.len());
                let at = rng.index(len + 1);
                case.blocks.splice(at..at, donor.blocks[start..end].iter().cloned());
                if let (Some(ours), Some(theirs)) = (&mut generator.preimages, &donor.preimages) {
                    ours.addresses.extend(theirs.addresses.clone());
                    ours.slots.extend(theirs.slots.clone());
                }
            }
            "emptyOut" => {
                // A block that deletes every account, or clears one storage trie slot by slot.
                let i = rng.index(len + 1);
                let model = Self::start_of(case, i);
                let ops: Vec<TrieOp> = if rng.chance(50) || model.accounts.is_empty() {
                    model.accounts.keys().map(|key| TrieOp::delete(*key)).collect()
                } else {
                    let keys: Vec<&B256> = model.accounts.keys().collect();
                    let key = *keys[rng.index(keys.len())];
                    model.accounts[&key]
                        .storage
                        .keys()
                        .map(|slot| TrieOp::storage(key, *slot, B256::ZERO))
                        .collect()
                };
                let proofs = generator.proofs(&model);
                case.blocks.insert(i, CaseBlock { unwind: 0, ops, proofs });
            }
            _ => {
                if len < 2 {
                    return false;
                }
                case.blocks.truncate(rng.range(1, len as u64 - 1) as usize);
            }
        }
        true
    }

    /// Moves one account key or one slot (everywhere it appears) next to another key of the
    /// case, so the two share a chosen number of leading nibbles.
    fn rekey(rng: &mut Rng, generator: &mut TrieGen, case: &mut TrieCase) -> bool {
        let accounts = case.account_keys();
        if accounts.is_empty() {
            return false;
        }
        let account = accounts[rng.index(accounts.len())];
        let slots = case.slots_of(&account);
        let rekey_account = slots.is_empty() || rng.chance(40);
        let shared = generator.shared_len().min(63);
        if rekey_account {
            let base = accounts[rng.index(accounts.len())];
            let new = generator.near(&base, shared, false);
            for block in &mut case.blocks {
                for op in &mut block.ops {
                    match op {
                        TrieOp::Account { key, .. }
                        | TrieOp::Storage { key, .. }
                        | TrieOp::Wipe { key, .. }
                        | TrieOp::Delete { key, .. } => {
                            if *key == account {
                                *key = new;
                            }
                        }
                    }
                }
                for proof in block.proofs.iter_mut().filter(|p| p.key == account) {
                    proof.key = new;
                }
            }
        } else {
            let old = slots[rng.index(slots.len())];
            let base = slots[rng.index(slots.len())];
            let new = generator.near(&base, shared, true);
            for block in &mut case.blocks {
                for op in &mut block.ops {
                    if let TrieOp::Storage { key, slot, .. } = op
                        && *key == account
                        && *slot == old
                    {
                        *slot = new;
                    }
                }
                for proof in block.proofs.iter_mut().filter(|p| p.key == account) {
                    proof.slots.iter_mut().filter(|s| **s == old).for_each(|s| *s = new);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutations_keep_cases_buildable() {
        let mut rng = Rng::new(&[11]);
        let donors: Vec<TrieCase> = (0..8).map(|i| TrieGen::new(rng.fork(i)).case()).collect();
        let mut with_preimages = 0;
        for seed in 0..64 {
            let mut case = TrieGen::new(Rng::new(&[seed])).case();
            for round in 0..6 {
                let donor = &donors[(seed as usize + round) % donors.len()];
                TrieMutator::mutate(&mut rng, &mut case, Some(donor));
            }
            let built = case.build(seed.to_string(), String::new());
            assert_eq!(built.expected.len(), case.blocks.len());
            assert!(!case.blocks.is_empty() && case.blocks.len() <= TrieMutator::MAX_BLOCKS);
            // A case generated with preimages keeps one for every key it names.
            if case.preimages.is_some() {
                with_preimages += 1;
                for block in &built.request.trie.blocks {
                    for op in block.revert.iter().chain(&block.ops) {
                        match op {
                            TrieOp::Storage { address, slot_key, .. } => {
                                assert!(address.is_some() && slot_key.is_some(), "{op:?}");
                            }
                            TrieOp::Account { address, .. }
                            | TrieOp::Wipe { address, .. }
                            | TrieOp::Delete { address, .. } => {
                                assert!(address.is_some(), "{op:?}")
                            }
                        }
                    }
                    for proof in &block.proofs {
                        assert!(proof.address.is_some(), "{proof:?}");
                        assert_eq!(proof.slot_keys.as_ref().map(Vec::len), Some(proof.slots.len()));
                    }
                }
            }
        }
        assert!(with_preimages > 0);
    }
}
