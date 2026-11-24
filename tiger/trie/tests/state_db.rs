//! The state database against an in-memory model: random blocks of account, storage, wipe and
//! delete changes, native unwinds, reopens and simulated crashes, with every root checked
//! against a from-scratch `HashBuilder` root and historical reads, roots, overlays and proofs
//! checked at random past blocks.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_trie::{HashBuilder, KECCAK_EMPTY, Nibbles, proof::verify_proof};
use base_tiger_trie::{AccountInfo, BlockChanges, StateDb, account_leaf, storage_leaf};

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

fn root(model: &Model) -> B256 {
    let mut sorted: Vec<(B256, &AccountInfo, &BTreeMap<U256, U256>)> =
        model.iter().map(|(a, (i, s))| (keccak256(a), i, s)).collect();
    sorted.sort_by_key(|(k, _, _)| *k);
    let mut builder = HashBuilder::default();
    for (key, info, storage) in sorted {
        let mut slots: Vec<(B256, U256)> = storage.iter().map(|(s, v)| (keccak256(s.to_be_bytes::<32>()), *v)).collect();
        slots.sort();
        let mut sb = HashBuilder::default();
        for (slot, value) in slots {
            sb.add_leaf(Nibbles::unpack(slot), &storage_leaf(value));
        }
        builder.add_leaf(Nibbles::unpack(key), &account_leaf(&info.to_account(), sb.root()));
    }
    builder.root()
}

fn hashed_nodes(proof: &[Bytes]) -> Vec<Bytes> {
    proof.iter().enumerate().filter(|(i, n)| *i == 0 || n.len() >= 32).map(|(_, n)| n.clone()).collect()
}

/// A random block over `addresses` and `slots`, applied to `model`.
fn random_block(rng: &mut Rng, model: &mut Model, addresses: &[Address], slots: &[U256]) -> BlockChanges {
    let mut changes = BlockChanges::default();
    for _ in 0..1 + rng.below(12) {
        let address = addresses[rng.below(addresses.len() as u64) as usize];
        match rng.below(10) {
            0..=3 => {
                let code_hash = if rng.below(3) == 0 { keccak256(rng.next().to_be_bytes()) } else { KECCAK_EMPTY };
                let info = AccountInfo { nonce: rng.below(5), balance: U256::from(1 + rng.below(1000)), code_hash };
                if code_hash != KECCAK_EMPTY {
                    changes.codes.push((code_hash, Bytes::from(rng.next().to_be_bytes().to_vec())));
                }
                changes.accounts.push((address, Some(info)));
                model.entry(address).or_insert((info, BTreeMap::new())).0 = info;
            }
            4..=7 => {
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
            8 => {
                // Wipe: earlier storage writes of this block to the address are dropped.
                if let Some((_, storage)) = model.get_mut(&address) {
                    storage.clear();
                    changes.storage.retain(|(a, _, _)| *a != address);
                    changes.wiped.push(address);
                }
            }
            _ => {
                if model.remove(&address).is_some() {
                    changes.storage.retain(|(a, _, _)| *a != address);
                    changes.accounts.push((address, None));
                    changes.wiped.push(address);
                }
            }
        }
    }
    // A delete followed by a recreate keeps the wipe; storage after the recreate stays.
    changes
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tiger-statedb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn check_view(db: &StateDb, block: u64, model: &Model, expected_root: B256, addresses: &[Address], slots: &[U256], rng: &mut Rng) {
    let view = db.view(block, Vec::new()).unwrap();
    assert_eq!(view.state_root(None).unwrap(), expected_root, "historical root at {block}");
    for address in addresses {
        let expected = model.get(address);
        assert_eq!(view.account(*address).unwrap(), expected.map(|(i, _)| *i), "account {address} at {block}");
        for slot in slots.iter().take(4) {
            let value = expected.and_then(|(_, s)| s.get(slot)).copied().unwrap_or_default();
            assert_eq!(view.storage(*address, *slot).unwrap(), value, "slot of {address} at {block}");
        }
    }
    let address = addresses[rng.below(addresses.len() as u64) as usize];
    let proof_slots = [slots[0], slots[rng.below(slots.len() as u64) as usize]];
    let proof = view.proof(address, &proof_slots).unwrap();
    let expected = model.get(&address);
    let value = expected.map(|(info, storage)| {
        let mut sb = HashBuilder::default();
        let mut s: Vec<(B256, U256)> = storage.iter().map(|(k, v)| (keccak256(k.to_be_bytes::<32>()), *v)).collect();
        s.sort();
        for (k, v) in s {
            sb.add_leaf(Nibbles::unpack(k), &storage_leaf(v));
        }
        account_leaf(&info.to_account(), sb.root())
    });
    verify_proof(expected_root, Nibbles::unpack(keccak256(address)), value, &hashed_nodes(&proof.proof)).unwrap();
    for sp in &proof.storage {
        let slot_value = expected.and_then(|(_, s)| s.iter().find(|(k, _)| keccak256(k.to_be_bytes::<32>()) == sp.slot)).map(|(_, v)| *v);
        assert_eq!(sp.value, slot_value.unwrap_or_default());
        if expected.is_some() {
            verify_proof(proof.storage_root, Nibbles::unpack(sp.slot), slot_value.map(storage_leaf), &hashed_nodes(&sp.proof)).unwrap();
        }
    }
}

#[test]
fn randomized_blocks_unwinds_reopens_and_history() {
    for seed in 1..=6u64 {
        let mut rng = Rng(seed);
        let dir = temp(&format!("rand-{seed}"));
        let addresses: Vec<Address> = (0..24u64).map(|i| Address::from_word(keccak256(i.to_be_bytes()))).collect();
        let slots: Vec<U256> = (0..12u64).map(|i| U256::from(i * 7 + seed)).collect();
        let mut db = StateDb::open(&dir).unwrap();
        // snapshots[n] = model after block n; changes[n] = block n's changes.
        let mut snapshots: Vec<Model> = Vec::new();
        let mut roots: Vec<B256> = Vec::new();
        let mut all_changes: Vec<BlockChanges> = Vec::new();
        let mut model = Model::new();
        for step in 0..40 {
            let tip = snapshots.len() as u64;
            if step > 3 && rng.below(6) == 0 {
                let depth = 1 + rng.below(3.min(tip - 1));
                let target = tip - 1 - depth;
                let got = db.unwind_to(target).unwrap();
                snapshots.truncate(target as usize + 1);
                roots.truncate(target as usize + 1);
                all_changes.truncate(target as usize + 1);
                model = snapshots.last().unwrap().clone();
                assert_eq!(got, roots[target as usize], "seed {seed} unwind to {target}");
                continue;
            }
            let number = snapshots.len() as u64;
            let changes = random_block(&mut rng, &mut model, &addresses, &slots);
            // Sometimes crash after the static files (and RocksDB) committed, then reopen.
            if rng.below(8) == 0 {
                let mut w = db.write().unwrap();
                db.commit_block(&mut w, number, &changes).unwrap();
                w.crash_before_mdbx(rng.below(2) == 0).unwrap();
                drop(db);
                db = StateDb::open(&dir).unwrap();
                assert_eq!(db.tip().unwrap(), number.checked_sub(1));
            }
            let mut w = db.write().unwrap();
            let got = db.commit_block(&mut w, number, &changes).unwrap();
            w.commit().unwrap();
            assert_eq!(got, root(&model), "seed {seed} block {number}");
            snapshots.push(model.clone());
            roots.push(got);
            all_changes.push(changes);
            if rng.below(5) == 0 {
                drop(db);
                db = StateDb::open(&dir).unwrap();
            }
        }
        let tip = snapshots.len() as u64 - 1;
        assert_eq!(db.tip().unwrap(), Some(tip));
        for _ in 0..6 {
            let block = rng.below(tip + 1);
            check_view(&db, block, &snapshots[block as usize], roots[block as usize], &addresses, &slots, &mut rng);
        }
        check_view(&db, tip, &snapshots[tip as usize], roots[tip as usize], &addresses, &slots, &mut rng);
        // In-memory overlays: the stored block two back with the last two blocks on top.
        let base = tip - 2;
        let view = db.view(base, vec![all_changes[tip as usize - 1].clone(), all_changes[tip as usize].clone()]).unwrap();
        assert_eq!(view.state_root(None).unwrap(), roots[tip as usize]);
        for address in &addresses {
            assert_eq!(view.account(*address).unwrap(), snapshots[tip as usize].get(address).map(|(i, _)| *i));
            for slot in &slots {
                let value = snapshots[tip as usize].get(address).and_then(|(_, s)| s.get(slot)).copied().unwrap_or_default();
                assert_eq!(view.storage(*address, *slot).unwrap(), value);
            }
        }
        drop(view);
        let view = db.view(base, vec![all_changes[tip as usize - 1].clone()]).unwrap();
        assert_eq!(view.state_root(Some(&all_changes[tip as usize])).unwrap(), roots[tip as usize]);
        drop(view);
        // Unwind all the way to genesis.
        assert_eq!(db.unwind_to(0).unwrap(), roots[0]);
        drop(db);
        let db = StateDb::open(&dir).unwrap();
        assert_eq!(db.view(0, Vec::new()).unwrap().state_root(None).unwrap(), roots[0]);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
