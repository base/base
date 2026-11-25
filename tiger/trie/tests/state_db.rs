//! The state database against an in-memory model: random blocks of account, storage, wipe and
//! delete changes, native unwinds, reopens and simulated crashes, with every root checked
//! against a from-scratch `HashBuilder` root and historical reads, roots, overlays and proofs
//! checked at random past blocks.

use std::collections::BTreeMap;

use alloy_consensus::{Header, Receipt, Signed, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, U256, keccak256};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use alloy_trie::{HashBuilder, KECCAK_EMPTY, Nibbles, proof::verify_proof};
use base_tiger_store::{RocksTable, Segment};
use base_tiger_trie::{AccountInfo, BlockChanges, MemState, PreparedRoot, StateDb, StateView, account_leaf, storage_leaf};

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

/// Block `number`'s header and `count` distinct legacy transactions with senders and receipts.
fn block_data(number: u64, root: B256, count: u64) -> (Header, Vec<BaseTxEnvelope>, Vec<Address>, Vec<BaseReceipt>) {
    let header = Header { number, state_root: root, ..Header::default() };
    let mut txs = Vec::new();
    let mut receipts = Vec::new();
    for i in 0..count {
        let tx = TxLegacy { nonce: number * 16 + i, gas_limit: 21_000, chain_id: Some(8453), ..TxLegacy::default() };
        let signature = Signature::new(U256::from(1 + i), U256::from(number + 1), false);
        txs.push(BaseTxEnvelope::Legacy(Signed::new_unhashed(tx, signature)));
        let receipt = Receipt { status: true.into(), cumulative_gas_used: 21_000 * (i + 1), logs: Vec::new() };
        receipts.push(BaseReceipt::Legacy(receipt));
    }
    let senders = (0..count).map(|i| Address::with_last_byte(i as u8)).collect();
    (header, txs, senders, receipts)
}

/// The stored block data agrees with the stored tip: blocks up to it and their transactions
/// are found, the given blocks above it are gone.
fn check_blocks(db: &StateDb, genesis: u64, tip: u64, gone: &[(u64, Vec<BaseTxEnvelope>)]) {
    let dd = &db.datadir;
    for number in genesis..=tip {
        let (header, hash) = dd.header(number).unwrap().unwrap();
        assert_eq!(dd.block_number(hash).unwrap(), Some(number));
        let txs = dd.transactions(number).unwrap().unwrap();
        assert_eq!(txs, block_data(number, header.state_root, txs.len() as u64).1);
        for (index, tx) in txs.iter().enumerate() {
            assert_eq!(dd.transaction_location(tx.trie_hash()).unwrap(), Some((number, index)));
        }
        assert_eq!(dd.receipts(number).unwrap().unwrap().len(), txs.len());
    }
    assert!(dd.header(tip + 1).unwrap().is_none());
    for (number, txs) in gone {
        assert!(*number > tip);
        for tx in txs {
            let entry = dd.rocks.get(RocksTable::TransactionHashNumbers, tx.trie_hash().as_slice()).unwrap();
            assert_eq!(entry, None, "hash of a tx of block {number} above the tip {tip}");
        }
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tiger-statedb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn check_view(db: &StateDb, block: u64, model: &Model, expected_root: B256, addresses: &[Address], slots: &[U256], rng: &mut Rng) {
    check_state(&db.view(block, Vec::new()).unwrap(), block, model, expected_root, addresses, slots, rng);
}

/// Roots, reads and proofs of `view` (the state after `block`) against `model`.
fn check_state(view: &StateView<'_>, block: u64, model: &Model, expected_root: B256, addresses: &[Address], slots: &[U256], rng: &mut Rng) {
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
        // Even seeds start at a genesis above zero, just before a static-file boundary.
        let genesis = if seed % 2 == 0 { seed * 1_000_000 + 499_990 } else { 0 };
        let mut db = StateDb::open(&dir).unwrap();
        // snapshots[n] = model after block genesis + n; changes[n] = that block's changes.
        let mut snapshots: Vec<Model> = Vec::new();
        let mut roots: Vec<B256> = Vec::new();
        let mut all_changes: Vec<BlockChanges> = Vec::new();
        let mut model = Model::new();
        for _ in 0..40 {
            let tip = snapshots.len() as u64;
            if tip > 2 && rng.below(6) == 0 {
                let depth = 1 + rng.below(3.min(tip - 1));
                let target = tip - 1 - depth;
                let gone: Vec<_> = (genesis + target + 1..genesis + tip)
                    .map(|n| (n, db.datadir.transactions(n).unwrap().unwrap()))
                    .collect();
                let got = db.unwind_to(genesis + target).unwrap();
                check_blocks(&db, genesis, genesis + target, &gone);
                snapshots.truncate(target as usize + 1);
                roots.truncate(target as usize + 1);
                all_changes.truncate(target as usize + 1);
                model = snapshots.last().unwrap().clone();
                assert_eq!(got, roots[target as usize], "seed {seed} unwind to {target}");
                continue;
            }
            let number = genesis + snapshots.len() as u64;
            let changes = random_block(&mut rng, &mut model, &addresses, &slots);
            // Sometimes crash after the static files (and RocksDB) committed, then reopen.
            let tx_count = rng.below(3);
            if rng.below(8) == 0 {
                let mut w = db.write().unwrap();
                let root = db.commit_block(&mut w, number, &changes).unwrap();
                let (header, txs, senders, receipts) = block_data(number, root, tx_count);
                w.append_block(&header, header.hash_slow(), &txs, &senders, &receipts).unwrap();
                w.crash_before_mdbx(rng.below(2) == 0).unwrap();
                drop(db);
                db = StateDb::open(&dir).unwrap();
                assert_eq!(db.tip().unwrap(), (number > genesis).then(|| number - 1));
                if number > genesis {
                    check_blocks(&db, genesis, number - 1, &[(number, txs)]);
                }
            }
            let mut w = db.write().unwrap();
            let got = db.commit_block(&mut w, number, &changes).unwrap();
            let (header, txs, senders, receipts) = block_data(number, got, tx_count);
            w.append_block(&header, header.hash_slow(), &txs, &senders, &receipts).unwrap();
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
        assert_eq!(db.tip().unwrap(), Some(genesis + tip));
        check_blocks(&db, genesis, genesis + tip, &[]);
        for _ in 0..6 {
            let block = rng.below(tip + 1);
            check_view(&db, genesis + block, &snapshots[block as usize], roots[block as usize], &addresses, &slots, &mut rng);
        }
        check_view(&db, genesis + tip, &snapshots[tip as usize], roots[tip as usize], &addresses, &slots, &mut rng);
        // In-memory overlays: the stored block two back with the last two blocks on top.
        let base = tip - 2;
        let view = db.view(genesis + base, vec![all_changes[tip as usize - 1].clone(), all_changes[tip as usize].clone()]).unwrap();
        assert_eq!(view.state_root(None).unwrap(), roots[tip as usize]);
        for address in &addresses {
            assert_eq!(view.account(*address).unwrap(), snapshots[tip as usize].get(address).map(|(i, _)| *i));
            for slot in &slots {
                let value = snapshots[tip as usize].get(address).and_then(|(_, s)| s.get(slot)).copied().unwrap_or_default();
                assert_eq!(view.storage(*address, *slot).unwrap(), value);
            }
        }
        drop(view);
        let view = db.view(genesis + base, vec![all_changes[tip as usize - 1].clone()]).unwrap();
        assert_eq!(view.state_root(Some(&all_changes[tip as usize])).unwrap(), roots[tip as usize]);
        drop(view);
        // Unwind all the way to genesis.
        assert_eq!(db.unwind_to(genesis).unwrap(), roots[0]);
        drop(db);
        let db = StateDb::open(&dir).unwrap();
        assert_eq!(db.view(genesis, Vec::new()).unwrap().state_root(None).unwrap(), roots[0]);
        assert!(genesis == 0 || db.view(genesis - 1, Vec::new()).and_then(|v| v.state_root(None)).is_err(), "no state below genesis");
        assert_eq!(db.datadir.static_files.first_block(Segment::AccountChangeSets), Some(genesis));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A root prepared while validating is reused at commit only while it still applies: a commit in
/// between (here an unwind and a different block at the same height) forces a recompute.
#[test]
fn prepared_roots_apply_only_to_the_state_they_were_computed_on() {
    let dir = temp("prepared");
    let mut rng = Rng(77);
    let addresses: Vec<Address> = (0..24u64).map(|i| Address::from_word(keccak256(i.to_be_bytes()))).collect();
    let slots: Vec<U256> = (0..12u64).map(U256::from).collect();
    let db = StateDb::open(&dir).unwrap();
    let mut model = Model::new();
    let commit = |number: u64, changes: &BlockChanges, prepared: Option<&PreparedRoot>| {
        let mut w = db.write().unwrap();
        let root = db.commit_prepared(&mut w, number, changes, prepared).unwrap();
        w.commit().unwrap();
        root
    };
    let genesis = random_block(&mut rng, &mut model, &addresses, &slots);
    commit(0, &genesis, None);
    for number in 1..8u64 {
        let changes = random_block(&mut rng, &mut model, &addresses, &slots);
        let prepared = db.view(number - 1, Vec::new()).unwrap().prepare_root(&changes).unwrap();
        assert_eq!(prepared.root, root(&model));
        assert_eq!(commit(number, &changes, Some(&prepared)), root(&model), "block {number}");
    }
    // Prepare block 8 on block 7, then replace block 7 by another block.
    let mut stale_model = model.clone();
    let next = random_block(&mut rng, &mut stale_model, &addresses, &slots);
    let prepared = db.view(7, Vec::new()).unwrap().prepare_root(&next).unwrap();
    db.unwind_to(6).unwrap();
    let mut other_model = Model::new();
    for address in &addresses {
        let view = db.view(6, Vec::new()).unwrap();
        if let Some(info) = view.account(*address).unwrap() {
            let storage = slots.iter().map(|s| (*s, view.storage(*address, *s).unwrap())).filter(|(_, v)| !v.is_zero()).collect();
            other_model.insert(*address, (info, storage));
        }
    }
    let other = random_block(&mut rng, &mut other_model, &addresses, &slots);
    commit(7, &other, None);
    let expected = db.view(7, Vec::new()).unwrap().state_root(Some(&next)).unwrap();
    assert_eq!(commit(8, &next, Some(&prepared)), expected);
    assert_eq!(db.view(8, Vec::new()).unwrap().state_root(None).unwrap(), expected);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A base-reth full node's datadir: no senders segment (`sender_recovery = "full"`) and history
/// pruned up to a checkpoint. Blocks commit on top without starting a senders segment, and views
/// below the checkpoint fail instead of reading the pruned history as the current state.
#[test]
fn pruned_senders_and_history() {
    let dir = temp("pruned");
    let mut rng = Rng(5);
    let addresses: Vec<Address> = (0..8u64).map(|i| Address::from_word(keccak256(i.to_be_bytes()))).collect();
    let slots: Vec<U256> = (0..4u64).map(U256::from).collect();
    let mut db = StateDb::open(&dir).unwrap();
    let mut model = Model::new();
    let mut commit = |db: &StateDb, number: u64| {
        let changes = random_block(&mut rng, &mut model, &addresses, &slots);
        let mut w = db.write().unwrap();
        let root = db.commit_block(&mut w, number, &changes).unwrap();
        let (header, txs, senders, receipts) = block_data(number, root, 2);
        w.append_block(&header, header.hash_slow(), &txs, &senders, &receipts).unwrap();
        w.commit().unwrap();
    };
    for number in 0..4 {
        commit(&db, number);
    }
    assert_eq!(db.datadir.senders(3).unwrap().map(|s| s.len()), Some(2));
    db.datadir.static_files.clear(Segment::TransactionSenders).unwrap();
    for number in 4..8 {
        commit(&db, number);
    }
    // reth's `AccountHistory` checkpoint at block 5, Distance(10064).
    {
        let mut txn = db.datadir.db.write().unwrap();
        let dbi = db.datadir.db.dbi(base_tiger_store::Table::PruneCheckpoints);
        txn.put(dbi, &[4], &[0x01, 0x01, 0x05, 0x01, 0x27, 0x50]).unwrap();
        txn.commit().unwrap();
    }
    drop(db);
    db = StateDb::open(&dir).unwrap();
    assert_eq!(db.tip().unwrap(), Some(7));
    assert_eq!(db.datadir.static_files.last_block(Segment::TransactionSenders), None);
    for number in 0..8 {
        assert_eq!(db.datadir.senders(number).unwrap(), None, "block {number}");
        assert_eq!(db.datadir.transactions(number).unwrap().unwrap().len(), 2);
    }
    assert_eq!(db.datadir.history_pruned_to().unwrap(), Some(5));
    assert!(db.view(4, Vec::new()).is_err());
    for number in 5..8 {
        db.view(number, Vec::new()).unwrap().account(addresses[0]).unwrap();
    }
}

/// Canonical blocks held in memory as one merged layer over the datadir, as the node keeps them
/// between persistence batches: reads, roots and proofs through the layer match the model at
/// every depth, also while a batch of the layer's blocks is committed under it, and the trie
/// changes each block's root computed over the layer commit in order to the same stored trie.
#[test]
fn in_memory_layer_reads_roots_and_batched_commits() {
    for seed in 1..=4u64 {
        let dir = temp(&format!("mem-{seed}"));
        let mut rng = Rng(100 + seed);
        let addresses: Vec<Address> = (0..24u64).map(|i| Address::from_word(keccak256(i.to_be_bytes()))).collect();
        let slots: Vec<U256> = (0..12u64).map(|i| U256::from(i * 3 + seed)).collect();
        let db = StateDb::open(&dir).unwrap();
        let mut model = Model::new();
        let genesis = random_block(&mut rng, &mut model, &addresses, &slots);
        let mut w = db.write().unwrap();
        db.commit_block(&mut w, 0, &genesis).unwrap();
        w.commit().unwrap();
        // Blocks 1..=30 in memory; blocks are persisted in batches as the node's persistence
        // thread does, the layer rebuilt from the blocks still in memory after each.
        let mut pending: Vec<(u64, BlockChanges, PreparedRoot)> = Vec::new();
        let mut mem = MemState::new(0);
        let mut roots = vec![root(&model)];
        let mut models = vec![model.clone()];
        for number in 1..=30u64 {
            let changes = random_block(&mut rng, &mut model, &addresses, &slots);
            let layer = std::sync::Arc::new(mem.clone());
            let view = db.view_mem(std::sync::Arc::clone(&layer), Vec::new()).unwrap();
            check_state(&view, number - 1, &models[number as usize - 1], roots[number as usize - 1], &addresses, &slots, &mut rng);
            let prepared = view.prepare_root(&changes).unwrap();
            assert_eq!(prepared.root, root(&model), "seed {seed} block {number}");
            assert_eq!(view.state_root(Some(&changes)).unwrap(), prepared.root);
            drop(view);
            mem.extend(number, prepared.hashed(), prepared.trie_updates().unwrap(), &changes.codes);
            pending.push((number, changes, prepared));
            roots.push(root(&model));
            models.push(model.clone());
            if rng.below(5) == 0 || number == 30 {
                // Commit all but the last few, then read through the layer of all of them over
                // the new tip.
                let layer = std::sync::Arc::new(mem.clone());
                let keep = if number == 30 { 0 } else { rng.below(4) as usize };
                let batch: Vec<_> = pending.drain(..pending.len().saturating_sub(keep)).collect();
                let mut w = db.write().unwrap();
                for (n, changes, prepared) in &batch {
                    assert_eq!(db.commit_prepared(&mut w, *n, changes, Some(prepared)).unwrap(), roots[*n as usize]);
                }
                w.commit().unwrap();
                let view = db.view_mem(std::sync::Arc::clone(&layer), Vec::new()).unwrap();
                let at = layer.head;
                check_state(&view, at, &models[at as usize], roots[at as usize], &addresses, &slots, &mut rng);
                drop(view);
                let tip = db.tip().unwrap().unwrap();
                mem = MemState::new(tip);
                for (n, changes, prepared) in &pending {
                    mem.extend(*n, prepared.hashed(), prepared.trie_updates().unwrap(), &changes.codes);
                }
                let view = db.view_mem(std::sync::Arc::new(mem.clone()), Vec::new()).unwrap();
                check_state(&view, number, &models[number as usize], roots[number as usize], &addresses, &slots, &mut rng);
            }
        }
        // The committed trie is the one a from-scratch walk finds, at the tip and below.
        assert_eq!(db.tip().unwrap(), Some(30));
        for block in [30, 29, 17, 3] {
            check_view(&db, block, &models[block as usize], roots[block as usize], &addresses, &slots, &mut rng);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
