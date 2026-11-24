//! A partial state filled only from per-block witnesses, against a full state database that
//! commits the same blocks: every root must match, every read must either match the full
//! state or fail as unknown (never answer a wrong value, zero or absent for state not held),
//! proofs of known keys must equal the full state's, and unwinds, reopens, overlays and
//! historical roots must agree.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_trie::{KECCAK_EMPTY, Nibbles};
use base_tiger_trie::{AccountInfo, BlockChanges, StateDb, StateView, TrieError};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

type Model = BTreeMap<Address, (AccountInfo, BTreeMap<U256, U256>)>;

/// A random block; wipes and deletions only hit accounts without storage (as on Base).
fn random_block(rng: &mut Rng, model: &mut Model, addresses: &[Address], slots: &[U256], size: u64) -> BlockChanges {
    let mut changes = BlockChanges::default();
    for _ in 0..1 + rng.below(size) {
        let address = addresses[rng.below(addresses.len() as u64) as usize];
        match rng.below(10) {
            0..=3 => {
                let code_hash = if rng.below(3) == 0 { keccak256(rng.next().to_be_bytes()) } else { KECCAK_EMPTY };
                let info = AccountInfo { nonce: rng.below(5), balance: U256::from(1 + rng.below(1000)), code_hash };
                if code_hash != KECCAK_EMPTY {
                    changes.codes.push((code_hash, rng.next().to_be_bytes().to_vec().into()));
                }
                changes.accounts.push((address, Some(info)));
                model.entry(address).or_insert((info, BTreeMap::new())).0 = info;
            }
            4..=8 => {
                if let Some((_, storage)) = model.get_mut(&address) {
                    let slot = slots[rng.below(slots.len() as u64) as usize];
                    let value = if rng.below(4) == 0 { U256::ZERO } else { U256::from(rng.below(1 << 40)) };
                    changes.storage.push((address, slot, value));
                    if value.is_zero() {
                        storage.remove(&slot);
                    } else {
                        storage.insert(slot, value);
                    }
                }
            }
            _ => {
                if model.get(&address).is_some_and(|(_, s)| s.is_empty()) && !changes.storage.iter().any(|(a, _, _)| *a == address) {
                    model.remove(&address);
                    changes.accounts.push((address, None));
                    changes.wiped.push(address);
                }
            }
        }
    }
    changes
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tiger-partial-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `partial` agrees with `full` wherever it answers; returns how many reads it answered.
fn compare_reads(full: &StateView<'_>, partial: &StateView<'_>, addresses: &[Address], slots: &[U256]) -> usize {
    let mut known = 0;
    let check = |what: String, err: TrieError| assert!(err.is_unknown(), "{what}: {err}");
    for address in addresses {
        match partial.account(*address) {
            Ok(info) => {
                known += 1;
                assert_eq!(info, full.account(*address).unwrap(), "account {address}");
            }
            Err(err) => check(format!("account {address}"), err),
        }
        for slot in slots {
            match partial.storage(*address, *slot) {
                Ok(value) => {
                    known += 1;
                    assert_eq!(value, full.storage(*address, *slot).unwrap(), "slot {slot} of {address}");
                }
                Err(err) => check(format!("slot {slot} of {address}"), err),
            }
        }
        match partial.proof(*address, &slots[..2]) {
            Ok(proof) => assert_eq!(proof, full.proof(*address, &slots[..2]).unwrap(), "proof of {address}"),
            Err(err) => check(format!("proof of {address}"), err),
        }
    }
    known
}

#[test]
fn partial_state_from_witnesses_matches_the_full_state() {
    for seed in 1..=8u64 {
        let mut rng = Rng(seed);
        let (full_dir, partial_dir) = (temp(&format!("full-{seed}")), temp(&format!("partial-{seed}")));
        let addresses: Vec<Address> = (0..40u64).map(|i| Address::from_word(keccak256((i + seed * 100).to_be_bytes()))).collect();
        let slots: Vec<U256> = (0..10u64).map(|i| U256::from(i * 3 + seed)).collect();
        let genesis = 1_000 * seed;
        let full = StateDb::open(&full_dir).unwrap();
        let mut model = Model::new();
        let changes = random_block(&mut rng, &mut model, &addresses, &slots, 120);
        let mut w = full.write().unwrap();
        let genesis_root = full.commit_block(&mut w, genesis, &changes).unwrap();
        w.commit().unwrap();
        let mut partial = StateDb::open_partial(&partial_dir, genesis, genesis_root).unwrap();
        assert!(partial.is_partial());

        // Nothing is known before the first witness.
        let view = partial.view(genesis, Vec::new()).unwrap();
        assert_eq!(view.state_root(None).unwrap(), genesis_root);
        assert!(view.account(addresses[0]).unwrap_err().is_unknown());
        drop(view);

        let mut roots = vec![genesis_root];
        let mut all_changes = vec![changes];
        let mut known_total = 0;
        for _ in 0..30 {
            let tip = genesis + roots.len() as u64 - 1;
            if roots.len() > 3 && rng.below(6) == 0 {
                let target = tip - 1 - rng.below(2);
                let expected = full.unwind_to(target).unwrap();
                assert_eq!(partial.unwind_to(target).unwrap(), expected, "seed {seed} unwind to {target}");
                roots.truncate((target - genesis) as usize + 1);
                all_changes.truncate((target - genesis) as usize + 1);
                assert_eq!(expected, roots[(target - genesis) as usize]);
                // The model follows the full database.
                let view = full.view(target, Vec::new()).unwrap();
                model.clear();
                for address in &addresses {
                    if let Some(info) = view.account(*address).unwrap() {
                        let storage = slots
                            .iter()
                            .map(|s| (*s, view.storage(*address, *s).unwrap()))
                            .filter(|(_, v)| !v.is_zero())
                            .collect();
                        model.insert(*address, (info, storage));
                    }
                }
                continue;
            }
            let number = tip + 1;
            let changes = random_block(&mut rng, &mut model, &addresses, &slots, 8);
            let witness = full.view(tip, Vec::new()).unwrap().trie_witness(&changes).unwrap();
            let mut w = full.write().unwrap();
            let root = full.commit_block(&mut w, number, &changes).unwrap();
            w.commit().unwrap();

            // Without the witness the partial state cannot commit (unless it already holds the paths).
            let mut w = partial.write().unwrap();
            partial.absorb_witness(&mut w, &witness, &[]).unwrap();
            assert_eq!(partial.commit_block(&mut w, number, &changes).unwrap(), root, "seed {seed} block {number}");
            w.commit().unwrap();
            roots.push(root);
            all_changes.push(changes.clone());

            let (fv, pv) = (full.view(number, Vec::new()).unwrap(), partial.view(number, Vec::new()).unwrap());
            assert_eq!(pv.state_root(None).unwrap(), root);
            known_total += compare_reads(&fv, &pv, &addresses, &slots);
            // Every account and slot the block wrote is known.
            for (address, _) in &changes.accounts {
                assert_eq!(pv.account(*address).unwrap(), fv.account(*address).unwrap());
            }
            for (address, slot, value) in &changes.storage {
                assert_eq!(pv.storage(*address, *slot).unwrap(), *value);
            }
            drop((fv, pv));

            if rng.below(4) == 0 {
                drop(partial);
                partial = StateDb::open_partial(&partial_dir, 0, B256::ZERO).unwrap();
            }
        }
        let tip = genesis + roots.len() as u64 - 1;
        // Overlays and historical roots.
        if roots.len() > 2 {
            let view = partial.view(tip - 1, vec![all_changes.last().unwrap().clone()]).unwrap();
            assert_eq!(view.state_root(None).unwrap(), roots[roots.len() - 1]);
            drop(view);
            for back in 1..roots.len().min(4) {
                let block = tip - back as u64;
                match partial.view(block, Vec::new()).unwrap().state_root(None) {
                    Ok(root) => assert_eq!(root, roots[(block - genesis) as usize], "historical root at {block}"),
                    Err(err) => assert!(err.is_unknown(), "historical root at {block}: {err}"),
                }
            }
        }
        assert!(known_total > 0);
        drop((full, partial));
        let _ = std::fs::remove_dir_all(&full_dir);
        let _ = std::fs::remove_dir_all(&partial_dir);
    }
}

/// A wipe of storage the partial state does not hold is refused as unknown, not guessed.
#[test]
fn partial_state_refuses_a_wipe_of_unknown_storage() {
    let (full_dir, partial_dir) = (temp("wipe-full"), temp("wipe-partial"));
    let address = Address::with_last_byte(7);
    let info = AccountInfo { nonce: 1, balance: U256::from(5), code_hash: KECCAK_EMPTY };
    let mut genesis = BlockChanges::default();
    genesis.accounts.push((address, Some(info)));
    genesis.storage.push((address, U256::from(1), U256::from(9)));
    let full = StateDb::open(&full_dir).unwrap();
    let mut w = full.write().unwrap();
    let root = full.commit_block(&mut w, 0, &genesis).unwrap();
    w.commit().unwrap();
    let partial = StateDb::open_partial(&partial_dir, 0, root).unwrap();
    let mut block = BlockChanges::default();
    block.accounts.push((address, Some(info)));
    block.wiped.push(address);
    let witness = full.view(0, Vec::new()).unwrap().trie_witness(&block).unwrap();
    let mut w = partial.write().unwrap();
    partial.absorb_witness(&mut w, &witness, &[]).unwrap();
    let err = partial.commit_block(&mut w, 1, &block).unwrap_err();
    assert!(err.is_unknown(), "{err}");
    drop(w);
    drop((full, partial));
    let _ = std::fs::remove_dir_all(&full_dir);
    let _ = std::fs::remove_dir_all(&partial_dir);
}

/// Proofs taken at an older root still fill the parts of the trie that did not change since,
/// reached through the stored nodes on the proved key's path; without the key paths the walk
/// stops at the changed root and nothing is stored.
#[test]
fn proofs_from_an_older_root_fill_unchanged_subtrees() {
    let (full_dir, partial_dir) = (temp("old-proofs-full"), temp("old-proofs-partial"));
    let info = |nonce| AccountInfo { nonce, balance: U256::from(1_000), code_hash: KECCAK_EMPTY };
    // Two accounts in different first-nibble subtrees, plus filler so the root is a branch.
    let addresses: Vec<Address> = (1..=40u8).map(Address::with_last_byte).collect();
    let changed = addresses[0];
    let unchanged = *addresses[1..]
        .iter()
        .find(|a| keccak256(a.as_slice())[0] >> 4 != keccak256(changed.as_slice())[0] >> 4)
        .unwrap();
    let mut genesis = BlockChanges::default();
    genesis.accounts.extend(addresses.iter().map(|a| (*a, Some(info(1)))));
    let full = StateDb::open(&full_dir).unwrap();
    let mut w = full.write().unwrap();
    full.commit_block(&mut w, 0, &genesis).unwrap();
    w.commit().unwrap();
    let mut block = BlockChanges::default();
    block.accounts.push((changed, Some(info(2))));
    let mut w = full.write().unwrap();
    let root = full.commit_block(&mut w, 1, &block).unwrap();
    w.commit().unwrap();

    let partial = StateDb::open_partial(&partial_dir, 1, root).unwrap();
    // The current root node and the changed account's path, from a proof at the tip.
    let current = full.view(1, Vec::new()).unwrap().proof(changed, &[]).unwrap();
    let mut w = partial.write().unwrap();
    partial.absorb_witness(&mut w, &current.proof, &[]).unwrap();
    w.commit().unwrap();
    let old = full.view(0, Vec::new()).unwrap().proof(unchanged, &[]).unwrap();
    assert!(partial.view(1, Vec::new()).unwrap().account(unchanged).unwrap_err().is_unknown());

    let mut w = partial.write().unwrap();
    partial.absorb_witness(&mut w, &old.proof, &[]).unwrap();
    w.commit().unwrap();
    assert!(partial.view(1, Vec::new()).unwrap().account(unchanged).unwrap_err().is_unknown());

    let keys = BTreeMap::from([(None, vec![Nibbles::unpack(keccak256(unchanged.as_slice()))])]);
    let mut w = partial.write().unwrap();
    partial.absorb_proofs(&mut w, &old.proof, &[], &keys).unwrap();
    w.commit().unwrap();
    assert_eq!(partial.view(1, Vec::new()).unwrap().account(unchanged).unwrap(), Some(info(1)));
    drop((full, partial));
    let _ = std::fs::remove_dir_all(&full_dir);
    let _ = std::fs::remove_dir_all(&partial_dir);
}
