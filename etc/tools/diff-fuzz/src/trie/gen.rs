//! Generation of trie cases aimed at trie shapes: keys chosen in hashed-key space to share
//! prefixes of every length, so writes and deletes split extensions, collapse branches, embed
//! short nodes, and empty whole tries.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_trie::KECCAK_EMPTY;

use crate::{CaseBlock, Preimages, ProofTarget, Rng, TrieCase, TrieModel, TrieOp};

/// Generates keys, values, ops, blocks, and cases. Keys come from pools that grow with keys
/// related to earlier ones.
#[derive(Debug)]
pub struct TrieGen {
    /// Random stream.
    pub rng: Rng,
    /// Account keys used so far.
    pub accounts: Vec<B256>,
    /// Storage slots used so far, shared across accounts.
    pub slots: Vec<B256>,
    /// Most leading nibbles two generated keys share: [`Self::REALISTIC_DEPTH`] for most
    /// cases, 64 for the deep ones.
    pub max_shared: usize,
    /// In preimage mode, the preimage of every key generated: keys are hashes of addresses and
    /// slot indices, aimed at shapes only through short shared prefixes found by search.
    pub preimages: Option<Preimages>,
}

impl TrieGen {
    /// The deepest shared prefix of two keccak keys a real trie plausibly holds (a search of
    /// about 2^48 hashes); deeper prefixes only occur in chosen keys.
    pub const REALISTIC_DEPTH: usize = 12;
    /// Percentage of fresh cases whose keys may share deeper prefixes.
    pub const DEEP_PERCENT: u64 = 20;

    /// A generator from a seed, with empty pools.
    pub const fn new(rng: Rng) -> Self {
        Self { rng, accounts: Vec::new(), slots: Vec::new(), max_shared: 64, preimages: None }
    }

    /// A generator whose pools hold every key and slot of `case`.
    pub fn for_case(rng: Rng, case: &TrieCase) -> Self {
        let accounts = case.account_keys();
        let mut slots = Vec::new();
        for key in &accounts {
            for slot in case.slots_of(key) {
                if !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
        }
        let max_shared =
            if case.depth() > Self::REALISTIC_DEPTH { 64 } else { Self::REALISTIC_DEPTH };
        Self { rng, accounts, slots, max_shared, preimages: case.preimages.clone() }
    }

    /// A number of shared leading nibbles: short prefixes split near the root, long ones put
    /// two keys under a deep extension or branch where their leaves embed. At most
    /// `max_shared`.
    pub fn shared_len(&mut self) -> usize {
        self.any_shared_len().min(self.max_shared)
    }

    fn any_shared_len(&mut self) -> usize {
        match self.rng.weighted(&[2, 6, 3, 1, 3, 1]) {
            0 => 0,
            1 => self.rng.range(1, 4) as usize,
            2 => self.rng.range(5, 16) as usize,
            3 => self.rng.range(17, 59) as usize,
            4 => self.rng.range(60, 63) as usize,
            _ => 64,
        }
    }

    /// A key sharing exactly `shared` leading nibbles with `base` (all 64 gives `base`). The
    /// rest is random, all zeros, all ones, or `base` with one more nibble changed.
    pub fn related(&mut self, base: &B256, shared: usize) -> B256 {
        if shared >= 64 {
            return *base;
        }
        let mut nibbles: Vec<u8> = base.iter().flat_map(|b| [b >> 4, b & 0x0f]).collect();
        // Patterned tails share long prefixes with each other; realistic cases use random ones.
        let tail = if self.max_shared > Self::REALISTIC_DEPTH { self.rng.below(4) } else { 0 };
        for (i, nibble) in nibbles.iter_mut().enumerate().skip(shared + 1) {
            *nibble = match tail {
                0 => self.rng.below(16) as u8,
                1 => 0,
                2 => 0x0f,
                _ if i == 63 => self.rng.below(16) as u8,
                _ => *nibble,
            };
        }
        let old = nibbles[shared];
        nibbles[shared] = (old + 1 + self.rng.below(15) as u8) % 16;
        let mut key = B256::ZERO;
        for (i, pair) in nibbles.chunks(2).enumerate() {
            key[i] = (pair[0] << 4) | pair[1];
        }
        key
    }

    fn pick_or_new(rng: &mut Rng, pool: &[B256]) -> Option<B256> {
        (!pool.is_empty()).then(|| pool[rng.index(pool.len())])
    }

    /// A key for a pool: an existing one, one related to an existing one, or a fresh one.
    fn key_from(&mut self, slots: bool, reuse: u64) -> B256 {
        let pool = if slots { &self.slots } else { &self.accounts };
        if let Some(existing) = Self::pick_or_new(&mut self.rng, pool) {
            if self.rng.chance(reuse) {
                return existing;
            }
            if self.rng.chance(75) {
                let shared = self.shared_len();
                return self.near(&existing, shared, slots);
            }
        }
        if self.preimages.is_some() {
            return self.preimage_key(slots, None);
        }
        let pick = if self.max_shared > Self::REALISTIC_DEPTH { self.rng.below(6) } else { 3 };
        let key = match pick {
            0 => B256::ZERO,
            1 => B256::repeat_byte(0xff),
            2 => B256::with_last_byte(self.rng.below(256) as u8),
            _ => self.rng.b256(),
        };
        self.remember(slots, key);
        key
    }

    /// A new key sharing `shared` leading nibbles with `base`: exactly, or in preimage mode at
    /// most three (found by search), with its preimage recorded.
    pub fn near(&mut self, base: &B256, shared: usize, slots: bool) -> B256 {
        if self.preimages.is_some() {
            return self.preimage_key(slots, Some((*base, shared.min(3))));
        }
        // Keys related to one base can coincide past the shared prefix; retry so no two keys of
        // a pool share more than `max_shared` nibbles, falling back to a random key.
        let mut key = self.related(base, shared);
        for attempt in 0..16 {
            let pool = if slots { &self.slots } else { &self.accounts };
            let deepest = pool
                .iter()
                .filter(|other| **other != key)
                .map(|other| U256::from_be_bytes((*other ^ key).0).leading_zeros() / 4)
                .max()
                .unwrap_or(0);
            if deepest <= self.max_shared {
                break;
            }
            // The prefix is crowded: fall back to a random key.
            key = if attempt == 15 { self.rng.b256() } else { self.related(base, shared) };
        }
        self.remember(slots, key);
        key
    }

    /// A hashed key with a preimage: an address, or a slot index (small, or random), optionally
    /// searched until the hash shares `shared` nibbles with a base key.
    fn preimage_key(&mut self, slots: bool, near: Option<(B256, usize)>) -> B256 {
        let small = slots && self.rng.chance(50);
        let mut best = None;
        for attempt in 0..(1u64 << 16) {
            let preimage = if small && near.is_none() {
                B256::from(U256::from(self.rng.below(64)).to_be_bytes::<32>())
            } else if slots {
                self.rng.b256()
            } else {
                B256::left_padding_from(self.rng.address().as_slice())
            };
            let hashed = if slots {
                keccak256(preimage)
            } else {
                keccak256(Address::from_slice(&preimage[12..]))
            };
            let common = near.map_or(0, |(base, _)| {
                let nibble = |k: &B256, i: usize| (k[i / 2] >> (4 * (1 - i % 2))) & 0x0f;
                (0..64).take_while(|i| nibble(&hashed, *i) == nibble(&base, *i)).count()
            });
            best = Some((hashed, preimage));
            if near.is_none_or(|(_, want)| common >= want) || attempt == (1 << 16) - 1 {
                break;
            }
        }
        let (hashed, preimage) = best.expect("at least one attempt");
        let preimages = self.preimages.as_mut().expect("preimage mode");
        if slots {
            preimages.slots.insert(hashed, preimage);
        } else {
            preimages.addresses.insert(hashed, Address::from_slice(&preimage[12..]));
        }
        self.remember(slots, hashed);
        hashed
    }

    fn remember(&mut self, slots: bool, key: B256) {
        let pool = if slots { &mut self.slots } else { &mut self.accounts };
        if !pool.contains(&key) {
            pool.push(key);
        }
    }

    /// An account key, mostly from the pool.
    pub fn account_key(&mut self) -> B256 {
        self.key_from(false, 55)
    }

    /// A storage slot, mostly from the pool.
    pub fn slot(&mut self) -> B256 {
        self.key_from(true, 50)
    }

    /// A storage value: small values make short leaves that embed in their parent.
    pub fn value(&mut self) -> B256 {
        let value = match self.rng.weighted(&[10, 3, 5, 1, 1]) {
            0 => U256::from(self.rng.range(1, 255)),
            1 => U256::from(self.rng.range(256, 0xffff)),
            2 => self.rng.u256(),
            3 => U256::MAX,
            _ => U256::from(1) << self.rng.below(256),
        };
        B256::from(value.max(U256::from(1)).to_be_bytes::<32>())
    }

    /// An account write of fresh fields.
    pub fn account(&mut self, key: B256) -> TrieOp {
        let nonce = match self.rng.below(8) {
            0 => 0,
            1 => u64::MAX,
            _ => self.rng.below(16),
        };
        let balance = match self.rng.below(6) {
            0 => U256::ZERO,
            1 => U256::MAX,
            2 => self.rng.u256(),
            _ => U256::from(self.rng.below(1 << 20)),
        };
        let code_hash = if self.rng.chance(60) { KECCAK_EMPTY } else { self.rng.b256() };
        TrieOp::account(key, nonce, balance, code_hash)
    }

    fn existing(&mut self, model: &TrieModel) -> Option<B256> {
        let keys: Vec<&B256> = model.accounts.keys().collect();
        (!keys.is_empty()).then(|| *keys[self.rng.index(keys.len())])
    }

    fn existing_slot(&mut self, model: &TrieModel, key: &B256) -> Option<B256> {
        let slots: Vec<&B256> = model.accounts.get(key)?.storage.keys().collect();
        (!slots.is_empty()).then(|| *slots[self.rng.index(slots.len())])
    }

    /// Appends one group of ops to `ops`, applying them to `model`.
    pub fn ops(&mut self, model: &mut TrieModel, ops: &mut Vec<TrieOp>) {
        let start = ops.len();
        let target = if self.rng.chance(70) { self.existing(model) } else { None };
        let key = target.unwrap_or_else(|| self.account_key());
        match self.rng.weighted(&[24, 30, 12, 6, 3, 6, 4, 2, 1, 2, 3, 2]) {
            0 => ops.push(self.account(key)),
            1 => {
                let slot = self.slot();
                let value = self.value();
                ops.push(TrieOp::storage(key, slot, value));
            }
            2 => {
                let slot = self.existing_slot(model, &key).unwrap_or_else(|| self.slot());
                ops.push(TrieOp::storage(key, slot, B256::ZERO));
            }
            3 => {
                // Written and set back within the block.
                let slot = self.existing_slot(model, &key).unwrap_or_else(|| self.slot());
                let before = model
                    .accounts
                    .get(&key)
                    .and_then(|a| a.storage.get(&slot))
                    .map_or(B256::ZERO, |v| B256::from(v.to_be_bytes::<32>()));
                let value = self.value();
                ops.push(TrieOp::storage(key, slot, value));
                ops.push(TrieOp::storage(key, slot, before));
            }
            4 => ops.push(TrieOp::wipe(key)),
            5 => ops.push(TrieOp::delete(key)),
            6 => {
                // Recreated: deleted and created again, with fresh storage.
                ops.push(TrieOp::delete(key));
                ops.push(self.account(key));
                for _ in 0..self.rng.below(3) {
                    let (slot, value) = (self.slot(), self.value());
                    ops.push(TrieOp::storage(key, slot, value));
                }
            }
            7 => {
                // One storage trie cleared slot by slot, down to empty.
                if let Some(account) = model.accounts.get(&key) {
                    for slot in account.storage.keys() {
                        ops.push(TrieOp::storage(key, *slot, B256::ZERO));
                    }
                }
            }
            8 => {
                // Every account deleted, down to the empty trie.
                for key in model.accounts.keys() {
                    ops.push(TrieOp::delete(*key));
                }
            }
            9 => {
                // Many writes under one storage trie, around one slot.
                let base = self.slot();
                for _ in 0..self.rng.range(16, 200) {
                    let shared = self.shared_len();
                    let slot = self.near(&base, shared, true);
                    let value = if self.rng.chance(15) { B256::ZERO } else { self.value() };
                    ops.push(TrieOp::storage(key, slot, value));
                }
            }
            10 => {
                // A whole subtree removed: every account, or every slot of one account, under
                // the first one to three nibbles of an existing key, so branches near the root
                // collapse.
                let nibbles = self.rng.weighted(&[5, 3, 1]) + 1;
                let account = model.accounts.get(&key);
                match account.zip(self.existing_slot(model, &key)) {
                    Some((account, slot)) if self.rng.chance(50) => {
                        for other in account.storage.keys().filter(|s| shares(s, &slot, nibbles)) {
                            ops.push(TrieOp::storage(key, *other, B256::ZERO));
                        }
                    }
                    _ => {
                        for other in model.accounts.keys().filter(|k| shares(k, &key, nibbles)) {
                            ops.push(TrieOp::delete(*other));
                        }
                    }
                }
            }
            _ => {
                // A wide fill: keys spread over every nibble, so the tries are populated well
                // below the root (and the sparse trie splits into lower subtries).
                let storage = self.rng.chance(50);
                for _ in 0..self.rng.range(32, 256) {
                    if storage {
                        let (slot, value) = (self.slot(), self.value());
                        ops.push(TrieOp::storage(key, slot, value));
                    } else {
                        let other = self.account_key();
                        ops.push(self.account(other));
                    }
                }
            }
        }
        for op in &ops[start..] {
            TrieCase::apply_normalized(model, op, &mut Vec::new());
        }
    }

    /// Proof reads: touched, untouched, and absent keys next to existing ones.
    pub fn proofs(&mut self, model: &TrieModel) -> Vec<ProofTarget> {
        let count = self.rng.weighted(&[3, 4, 2, 1]);
        (0..count)
            .map(|_| {
                let key = match (self.rng.below(10), self.existing(model)) {
                    (0..=5, Some(key)) => key,
                    (6..=8, Some(key)) => {
                        let shared = self.shared_len().min(63);
                        self.near(&key, shared, false)
                    }
                    _ => self.account_key(),
                };
                let slots = (0..self.rng.weighted(&[3, 3, 2, 1]))
                    .map(|_| match self.existing_slot(model, &key) {
                        Some(slot) if self.rng.chance(50) => slot,
                        Some(slot) => {
                            let shared = self.shared_len().min(63);
                            self.near(&slot, shared, true)
                        }
                        None => self.slot(),
                    })
                    .collect();
                ProofTarget { key, slots, ..Default::default() }
            })
            .collect()
    }

    /// One block on top of `model`, which it updates.
    pub fn block(&mut self, model: &mut TrieModel) -> CaseBlock {
        let groups = match self.rng.weighted(&[5, 4, 1]) {
            0 => self.rng.range(1, 4),
            1 => self.rng.range(5, 24),
            _ => self.rng.range(25, 120),
        };
        let mut ops = Vec::new();
        for _ in 0..groups {
            self.ops(model, &mut ops);
        }
        let proofs = self.proofs(model);
        CaseBlock { unwind: 0, ops, proofs }
    }

    /// A fresh case.
    pub fn case(&mut self) -> TrieCase {
        let blocks = match self.rng.weighted(&[3, 5, 2]) {
            0 => self.rng.range(1, 3),
            1 => self.rng.range(4, 10),
            _ => self.rng.range(11, 30),
        };
        let mut case = TrieCase::default();
        self.max_shared =
            if self.rng.chance(Self::DEEP_PERCENT) { 64 } else { Self::REALISTIC_DEPTH };
        if self.rng.chance(25) {
            self.preimages = Some(Preimages::default());
        }
        let mut states = vec![TrieModel::default()];
        for _ in 0..blocks {
            let mut unwind = 0;
            if states.len() > 2 && self.rng.chance(6) {
                unwind = self.rng.range(1, (states.len() as u64 - 1).min(3));
                states.truncate(states.len() - unwind as usize);
            }
            let mut model = states.last().expect("the empty state stays").clone();
            let mut block = self.block(&mut model);
            block.unwind = unwind;
            case.blocks.push(block);
            states.push(model);
        }
        case.preimages = self.preimages.clone();
        case
    }
}

/// Whether `a` and `b` share their first `nibbles` nibbles.
fn shares(a: &B256, b: &B256, nibbles: usize) -> bool {
    let bytes = nibbles / 2;
    a[..bytes] == b[..bytes] && (nibbles.is_multiple_of(2) || a[bytes] >> 4 == b[bytes] >> 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_keys_share_exactly_the_requested_prefix() {
        let mut generator = TrieGen::new(Rng::new(&[3]));
        let base = B256::repeat_byte(0x5a);
        for shared in 0..64 {
            let key = generator.related(&base, shared);
            let nibbles = |k: &B256| k.iter().flat_map(|b| [b >> 4, b & 0x0f]).collect::<Vec<_>>();
            let common =
                nibbles(&key).iter().zip(nibbles(&base)).take_while(|(a, b)| **a == *b).count();
            assert_eq!(common, shared);
        }
        assert_eq!(generator.related(&base, 64), base);
    }

    #[test]
    fn most_cases_stay_within_realistic_depth() {
        let mut realistic = 0;
        for seed in 0..40 {
            let mut generator = TrieGen::new(Rng::new(&[seed, 1]));
            let case = generator.case();
            if generator.max_shared == TrieGen::REALISTIC_DEPTH {
                realistic += 1;
                assert!(case.depth() <= TrieGen::REALISTIC_DEPTH, "seed {seed}: {}", case.depth());
            }
        }
        assert!(realistic >= 24, "{realistic} of 40 realistic");
    }

    #[test]
    fn generated_cases_build_and_match_their_replay() {
        for seed in 0..6 {
            let case = TrieGen::new(Rng::new(&[seed])).case();
            let built = case.build(format!("{seed}"), String::new());
            assert_eq!(built.expected.len(), case.blocks.len());
            let (_, last) = case.replay();
            let mut model = TrieModel::default();
            // The final state is the last block's start plus its ops.
            for block in &built.request.trie.blocks {
                for op in block.revert.iter().chain(&block.ops) {
                    model.apply(op);
                }
            }
            assert_eq!(model, last, "seed {seed}");
        }
    }
}
