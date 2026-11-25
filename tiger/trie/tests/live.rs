//! The live trie against the walker, end to end through the state database: random blocks of
//! transactions (account, storage, wipe and delete changes, values changed and changed back) are
//! streamed into a live session while "executing", then committed with the session's trie
//! updates into one datadir and with the walker's into another. Every root must match the
//! walker's, and the two datadirs' trie tables must stay byte-identical.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, U256, keccak256};
use alloy_trie::KECCAK_EMPTY;
use base_tiger_store::Table;
use base_tiger_trie::{AccountInfo, BlockChanges, StateDb};

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

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tiger-live-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Every row of the account and storage trie tables, raw.
fn trie_tables(db: &StateDb) -> Vec<(Vec<u8>, Vec<u8>)> {
    let d = &db.datadir.db;
    let txn = d.read().unwrap();
    let mut rows = Vec::new();
    for table in [Table::AccountsTrie, Table::StoragesTrie] {
        let mut cursor = txn.cursor(d.dbi(table)).unwrap();
        let mut entry = cursor.first().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
        while let Some(row) = entry {
            rows.push(row);
            entry = cursor.next().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
        }
    }
    rows
}

fn info(rng: &mut Rng) -> AccountInfo {
    let code_hash =
        if rng.below(3) == 0 { keccak256(rng.next().to_be_bytes()) } else { KECCAK_EMPTY };
    AccountInfo { nonce: rng.below(5), balance: U256::from(1 + rng.below(1000)), code_hash }
}

/// One random transaction over `model`, applied to it, with the addresses it wiped.
fn transaction(
    rng: &mut Rng,
    model: &mut Model,
    parent: &Model,
    addresses: &[Address],
    slots: &[U256],
) -> (BlockChanges, BTreeSet<Address>) {
    let mut tx = BlockChanges::default();
    let mut wiped = BTreeSet::new();
    for _ in 0..1 + rng.below(6) {
        let address = addresses[rng.below(addresses.len() as u64) as usize];
        match rng.below(20) {
            0..=6 => {
                let info = info(rng);
                tx.accounts.push((address, Some(info)));
                model.entry(address).or_insert((info, BTreeMap::new())).0 = info;
            }
            7..=15 if model.contains_key(&address) => {
                let slot = slots[rng.below(slots.len() as u64) as usize];
                // Sometimes back to the parent's value: a change the block's final changes drop.
                let value = match rng.below(5) {
                    0 => parent
                        .get(&address)
                        .and_then(|(_, s)| s.get(&slot))
                        .copied()
                        .unwrap_or_default(),
                    1 => U256::ZERO,
                    _ => U256::from(rng.below(1 << 40)),
                };
                tx.storage.push((address, slot, value));
                let storage = &mut model.get_mut(&address).unwrap().1;
                if value.is_zero() {
                    storage.remove(&slot);
                } else {
                    storage.insert(slot, value);
                }
            }
            16 if model.contains_key(&address) => {
                // Storage cleared, then written again.
                tx.wiped.push(address);
                wiped.insert(address);
                let storage = &mut model.get_mut(&address).unwrap().1;
                storage.clear();
                for _ in 0..rng.below(3) {
                    let slot = slots[rng.below(slots.len() as u64) as usize];
                    let value = U256::from(1 + rng.below(1000));
                    tx.storage.push((address, slot, value));
                    storage.insert(slot, value);
                }
            }
            17 if model.contains_key(&address) => {
                tx.accounts.push((address, None));
                wiped.insert(address);
                model.remove(&address);
            }
            _ => {}
        }
    }
    (tx, wiped)
}

/// The block's final changes from the parent and the post state, as the block-state accumulator
/// reports them: touched accounts and slots with their final values, wiped storages in full; with
/// `drop_unchanged`, values equal to the parent's are left out.
fn final_changes(
    parent: &Model,
    post: &Model,
    touched: &BTreeMap<Address, BTreeSet<U256>>,
    wiped: &BTreeSet<Address>,
    drop_unchanged: bool,
) -> BlockChanges {
    let mut out = BlockChanges::default();
    for (address, slots) in touched {
        let before = parent.get(address);
        let after = post.get(address);
        let changed = before.map(|(i, _)| i) != after.map(|(i, _)| i);
        if changed || !drop_unchanged || wiped.contains(address) {
            out.accounts.push((*address, after.map(|(i, _)| *i)));
        }
        let Some((_, storage)) = after else { continue };
        if wiped.contains(address) {
            out.wiped.push(*address);
            out.storage.extend(storage.iter().map(|(s, v)| (*address, *s, *v)));
            continue;
        }
        for slot in slots {
            let value = storage.get(slot).copied().unwrap_or_default();
            let was = before.and_then(|(_, s)| s.get(slot)).copied().unwrap_or_default();
            if value != was || !drop_unchanged {
                out.storage.push((*address, *slot, value));
            }
        }
    }
    out
}

#[test]
fn streamed_live_roots_commit_the_same_tries_as_the_walker() {
    for seed in 0..8u64 {
        let mut rng = Rng(seed);
        let (live_dir, walker_dir) =
            (temp(&format!("live-{seed}")), temp(&format!("walker-{seed}")));
        let mut live = StateDb::open(&live_dir).unwrap();
        live.set_live_root(true);
        let mut walker = StateDb::open(&walker_dir).unwrap();
        walker.set_live_root(false);
        let addresses: Vec<Address> =
            (0..400u64).map(|i| Address::from_slice(&keccak256(i.to_be_bytes())[12..])).collect();
        let slots: Vec<U256> = (0..60u64).map(U256::from).collect();
        // Genesis: most addresses with a little storage.
        let mut model = Model::new();
        let mut genesis = BlockChanges::default();
        for address in &addresses[..300] {
            let info = info(&mut rng);
            genesis.accounts.push((*address, Some(info)));
            let mut storage = BTreeMap::new();
            for _ in 0..rng.below(20) {
                let (slot, value) =
                    (slots[rng.below(60) as usize], U256::from(1 + rng.below(1 << 30)));
                genesis.storage.push((*address, slot, value));
                storage.insert(slot, value);
            }
            model.insert(*address, (info, storage));
        }
        for db in [&live, &walker] {
            let mut w = db.write().unwrap();
            db.commit_block(&mut w, 0, &genesis).unwrap();
            w.commit().unwrap();
        }
        for number in 1..=40u64 {
            let parent = model.clone();
            let view = live.view(number - 1, Vec::new()).unwrap();
            // Sometimes nothing streams (a block built without the hook).
            let session = if rng.below(6) == 0 { None } else { view.live_session() };
            // How often streamed transactions are applied before the next is sent: after each,
            // now and then, or never (the final changes do all the work).
            let flush_every = [1, 4, u64::MAX][rng.below(3) as usize];
            let mut touched: BTreeMap<Address, BTreeSet<U256>> = BTreeMap::new();
            let mut wiped = BTreeSet::new();
            for _ in 0..1 + rng.below(if number % 9 == 0 { 120 } else { 15 }) {
                let (tx, tx_wiped) = transaction(&mut rng, &mut model, &parent, &addresses, &slots);
                for (address, _) in &tx.accounts {
                    touched.entry(*address).or_default();
                }
                for (address, slot, _) in &tx.storage {
                    touched.entry(*address).or_default().insert(*slot);
                }
                wiped.extend(tx_wiped);
                if let Some(session) = &session {
                    session.transaction(tx);
                    if rng.below(flush_every) == 0 {
                        session.flush();
                    }
                }
            }
            let changes = final_changes(&parent, &model, &touched, &wiped, rng.below(2) == 0);
            let prepared = view.prepare_root_with(&changes, session).unwrap();
            drop(view);
            let mut w = live.write().unwrap();
            let live_root =
                live.commit_prepared(&mut w, number, &changes, Some(&prepared)).unwrap();
            w.commit().unwrap();
            let mut w = walker.write().unwrap();
            let walker_root = walker.commit_block(&mut w, number, &changes).unwrap();
            w.commit().unwrap();
            assert_eq!(prepared.root, walker_root, "seed {seed} block {number}");
            assert_eq!(live_root, walker_root, "seed {seed} block {number}");
            assert_eq!(
                trie_tables(&live),
                trie_tables(&walker),
                "seed {seed} block {number}: trie tables"
            );
        }
        drop((live, walker));
        let _ = std::fs::remove_dir_all(live_dir);
        let _ = std::fs::remove_dir_all(walker_dir);
    }
}
