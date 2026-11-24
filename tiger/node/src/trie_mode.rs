//! The `trie` mode of the differential fuzz adapter (etc/tools/diff-fuzz/TRIE_PROTOCOL.md):
//! apply blocks of hashed-key state writes through tiger's state database on a real datadir
//! under `trie.dir`, and report roots and proofs after every block.
//!
//! The primary path is what the node uses. With preimages on every op and proof target it is
//! [`StateDb::commit_block`] keyed by address and slot (changesets, history, native unwinds);
//! without them it is [`StateDb::commit_hashed`], and an unwind applies the request's `revert`
//! writes with the block's `ops` as one block.
//!
//! Variants, each on its own datadir:
//! - `reopen`: the datadir is closed and reopened after every block.
//! - `crash-heal`: every block is first committed up to, but not including, the MDBX commit
//!   (static files, and RocksDB on odd blocks), as a crash would leave it; the datadir is reopened
//!   (healing), then the block is committed for real.
//! - `torn-tail` (preimages): after every block, garbage is appended to every static file, as a
//!   torn uncommitted append leaves it; reopening must cut it off.
//! - `overlay` (preimages): before each block is written, its root as an in-memory overlay over
//!   the parent.
//! - `history` (preimages): after the last block, every canonical block's root, storage roots
//!   and proofs read back through historical views.
//!
//! Any difference after a reopen or heal is a bug.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    str::FromStr,
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_tiger_trie::{AccountInfo, AccountProof, BlockChanges, HashedState, StateDb, StateView, TrieError};
use serde_json::{Value, json};

/// One write of a request block.
#[derive(Debug, Clone)]
enum Op {
    Account { key: B256, address: Option<Address>, info: AccountInfo, code: Option<Bytes> },
    Storage { key: B256, address: Option<Address>, slot: B256, slot_key: Option<B256>, value: U256 },
    Wipe { key: B256, address: Option<Address> },
    Delete { key: B256, address: Option<Address> },
}

impl Op {
    fn key(&self) -> B256 {
        match self {
            Self::Account { key, .. } | Self::Storage { key, .. } | Self::Wipe { key, .. } | Self::Delete { key, .. } => *key,
        }
    }

    fn has_preimages(&self) -> bool {
        match self {
            Self::Storage { address, slot_key, .. } => address.is_some() && slot_key.is_some(),
            Self::Account { address, .. } | Self::Wipe { address, .. } | Self::Delete { address, .. } => address.is_some(),
        }
    }
}

/// A proof request.
#[derive(Debug, Clone)]
struct Target {
    key: B256,
    slots: Vec<B256>,
}

/// One request block.
#[derive(Debug, Clone)]
struct Block {
    number: u64,
    unwind: u64,
    revert: Vec<Op>,
    ops: Vec<Op>,
    proofs: Vec<Target>,
    /// Every key an op (or revert) names, for `storageRoots`.
    named: BTreeSet<B256>,
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    value.get(name).ok_or_else(|| format!("missing {name}"))
}

fn parse<T: FromStr>(value: &Value, name: &str) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    let text = field(value, name)?.as_str().ok_or_else(|| format!("{name} is not a string"))?;
    T::from_str(text).map_err(|err| format!("bad {name} {text:?}: {err}"))
}

fn parse_opt<T: FromStr>(value: &Value, name: &str) -> Result<Option<T>, String>
where
    T::Err: std::fmt::Display,
{
    if value.get(name).is_none_or(Value::is_null) { Ok(None) } else { parse(value, name).map(Some) }
}

fn quantity(value: &Value, name: &str) -> Result<u64, String> {
    match field(value, name)? {
        Value::Number(n) => n.as_u64().ok_or_else(|| format!("bad {name}")),
        _ => {
            let n: U256 = parse(value, name)?;
            u64::try_from(n).map_err(|_| format!("{name} out of range"))
        }
    }
}

fn parse_op(value: &Value) -> Result<Op, String> {
    let key: B256 = parse(value, "key")?;
    let address: Option<Address> = parse_opt(value, "address")?;
    if let Some(address) = address
        && keccak256(address) != key
    {
        return Err(format!("address {address} does not hash to {key}"));
    }
    let kind = field(value, "op")?.as_str().ok_or("op is not a string")?;
    Ok(match kind {
        "account" => {
            let info = AccountInfo {
                nonce: quantity(value, "nonce")?,
                balance: parse(value, "balance")?,
                code_hash: parse(value, "codeHash")?,
            };
            let code: Option<Bytes> = parse_opt(value, "code")?;
            if let Some(code) = &code
                && keccak256(code) != info.code_hash
            {
                return Err("code does not hash to codeHash".into());
            }
            Op::Account { key, address, info, code }
        }
        "storage" => {
            let slot: B256 = parse(value, "slot")?;
            let slot_key: Option<B256> = parse_opt(value, "slotKey")?;
            if let Some(slot_key) = slot_key
                && keccak256(slot_key) != slot
            {
                return Err(format!("slotKey {slot_key} does not hash to {slot}"));
            }
            let raw: B256 = parse(value, "value")?;
            Op::Storage { key, address, slot, slot_key, value: U256::from_be_bytes(raw.0) }
        }
        "wipe" => Op::Wipe { key, address },
        "delete" => Op::Delete { key, address },
        other => return Err(format!("unknown op {other:?}")),
    })
}

fn parse_blocks(trie: &Value) -> Result<(Vec<Block>, bool), String> {
    let mut blocks = Vec::new();
    let mut preimages = true;
    for value in field(trie, "blocks")?.as_array().ok_or("blocks is not an array")? {
        let ops_of = |name: &str| -> Result<Vec<Op>, String> {
            match value.get(name) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(list) => list.as_array().ok_or(format!("{name} is not an array"))?.iter().map(parse_op).collect(),
            }
        };
        let revert = ops_of("revert")?;
        let ops = ops_of("ops")?;
        let mut proofs = Vec::new();
        for target in value.get("proofs").and_then(Value::as_array).into_iter().flatten() {
            let key: B256 = parse(target, "key")?;
            let slots: Vec<B256> = target
                .get("slots")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|s| s.as_str().ok_or("slot is not a string".to_owned()).and_then(|s| B256::from_str(s).map_err(|e| e.to_string())))
                .collect::<Result<_, _>>()?;
            let address: Option<Address> = parse_opt(target, "address")?;
            let has_slot_keys = target.get("slotKeys").is_some_and(|k| !k.is_null());
            preimages &= address.is_some() && (slots.is_empty() || has_slot_keys);
            proofs.push(Target { key, slots });
        }
        preimages &= revert.iter().chain(&ops).all(Op::has_preimages);
        let named = revert.iter().chain(&ops).map(Op::key).collect();
        let unwind = if value.get("unwind").is_some() { quantity(value, "unwind")? } else { 0 };
        blocks.push(Block { number: quantity(value, "number")?, unwind, revert, ops, proofs, named });
    }
    Ok((blocks, preimages))
}

/// The ops as hashed state changes, applied in order.
fn hashed_changes<'a>(ops: impl IntoIterator<Item = &'a Op>) -> HashedState {
    let mut state = HashedState::default();
    for op in ops {
        match op {
            Op::Account { key, info, .. } => state.set_account(*key, info.to_account()),
            Op::Storage { key, slot, value, .. } => state.set_slot(*key, *slot, *value),
            Op::Wipe { key, .. } => state.wipe_storage(*key),
            Op::Delete { key, .. } => state.delete_account(*key),
        }
    }
    state
}

/// The ops as address-keyed block changes, applied in order (preimages required).
fn block_changes<'a>(ops: impl IntoIterator<Item = &'a Op>) -> BlockChanges {
    let mut accounts: BTreeMap<Address, Option<AccountInfo>> = BTreeMap::new();
    let mut storage: BTreeMap<(Address, U256), U256> = BTreeMap::new();
    let mut wiped: BTreeSet<Address> = BTreeSet::new();
    let mut codes: BTreeMap<B256, Bytes> = BTreeMap::new();
    for op in ops {
        match op {
            Op::Account { address, info, code, .. } => {
                accounts.insert(address.expect("preimage"), Some(*info));
                if let Some(code) = code {
                    codes.insert(info.code_hash, code.clone());
                }
            }
            Op::Storage { address, slot_key, value, .. } => {
                storage.insert((address.expect("preimage"), U256::from_be_bytes(slot_key.expect("preimage").0)), *value);
            }
            Op::Wipe { address, .. } => {
                let address = address.expect("preimage");
                storage.retain(|(a, _), _| *a != address);
                wiped.insert(address);
            }
            Op::Delete { address, .. } => {
                let address = address.expect("preimage");
                storage.retain(|(a, _), _| *a != address);
                accounts.insert(address, None);
                wiped.insert(address);
            }
        }
    }
    BlockChanges {
        accounts: accounts.into_iter().collect(),
        storage: storage.into_iter().map(|((a, s), v)| (a, s, v)).collect(),
        wiped: wiped.into_iter().collect(),
        codes: codes.into_iter().collect(),
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn hex_quantity(value: U256) -> String {
    format!("{value:#x}")
}

/// One block's result from a view at that block.
fn block_result(view: &StateView<'_>, block: &Block, root: B256, with_code: bool) -> Result<Value, TrieError> {
    let mut storage_roots = serde_json::Map::new();
    for key in &block.named {
        if view.account_hashed(*key)?.is_some() {
            storage_roots.insert(format!("{key:#x}"), json!(format!("{:#x}", view.storage_root_hashed(*key)?)));
        }
    }
    let mut proofs = Vec::with_capacity(block.proofs.len());
    for target in &block.proofs {
        let proof: AccountProof = view.proof_hashed(target.key, &target.slots)?;
        let account = proof.account.map(|a| {
            let info = AccountInfo::from_account(a);
            json!({
                "nonce": hex_quantity(U256::from(info.nonce)),
                "balance": hex_quantity(info.balance),
                "codeHash": format!("{:#x}", info.code_hash),
                "storageRoot": format!("{:#x}", proof.storage_root),
            })
        });
        let storage: Vec<Value> = proof
            .storage
            .iter()
            .map(|s| {
                json!({
                    "slot": format!("{:#x}", s.slot),
                    "value": format!("{:#x}", B256::from(s.value.to_be_bytes::<32>())),
                    "proof": s.proof.iter().map(|n| format!("{n:#x}")).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut entry = json!({
            "key": format!("{:#x}", target.key),
            "account": account,
            "accountProof": proof.proof.iter().map(|n| format!("{n:#x}")).collect::<Vec<_>>(),
            "storage": storage,
        });
        if with_code && let Some(a) = proof.account {
            let info = AccountInfo::from_account(a);
            if let Some(code) = view.code(info.code_hash)? {
                entry["code"] = json!(format!("{code:#x}"));
            }
        }
        proofs.push(entry);
    }
    Ok(json!({
        "number": format!("{:#x}", block.number),
        "stateRoot": format!("{root:#x}"),
        "storageRoots": storage_roots,
        "proofs": proofs,
    }))
}

/// How a run treats its datadir between blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Primary,
    Reopen,
    CrashHeal,
    TornTail,
}

/// Appends garbage to every static file, as a torn append that was never committed leaves it.
fn tear_static_files(dir: &Path, salt: u64) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir.join("static_files"))? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !name.starts_with("static_file_") || name.ends_with(".conf") {
            continue;
        }
        let len = 1 + (keccak256(format!("{name}{salt}")).0[0] % 23) as usize;
        OpenOptions::new().append(true).open(&path)?.write_all(&vec![0xa5; len])?;
    }
    Ok(())
}

/// Applies every block on a fresh datadir at `dir`; returns one result per block.
fn run(dir: &Path, blocks: &[Block], preimages: bool, mode: Run) -> Result<Vec<Value>, String> {
    let mut slot = Some(StateDb::open(dir).map_err(err)?);
    let db = slot.as_ref().expect("open");
    // Genesis: an empty block 0.
    let mut w = db.write().map_err(err)?;
    if preimages {
        db.commit_block(&mut w, 0, &BlockChanges::default()).map_err(err)?;
    } else {
        db.commit_hashed(&mut w, 0, &HashedState::default()).map_err(err)?;
    }
    w.commit().map_err(err)?;
    let mut results = Vec::with_capacity(blocks.len());
    let mut tip = 0u64;
    for (index, block) in blocks.iter().enumerate() {
        let step = catch_unwind(AssertUnwindSafe(|| -> Result<Value, String> {
            if block.number != tip + 1 - block.unwind {
                return Err(format!("block {} does not follow {tip} unwinding {}", block.number, block.unwind));
            }
            let root = if preimages {
                if block.unwind > 0 {
                    slot.as_ref().expect("open").unwind_to(block.number - 1).map_err(err)?;
                }
                let changes = block_changes(&block.ops);
                if mode == Run::CrashHeal {
                    let mut w = slot.as_ref().expect("open").write().map_err(err)?;
                    slot.as_ref().expect("open").commit_block(&mut w, block.number, &changes).map_err(err)?;
                    w.crash_before_mdbx(index % 2 == 1).map_err(err)?;
                    reopen(&mut slot, dir)?;
                }
                let mut w = slot.as_ref().expect("open").write().map_err(err)?;
                let root = slot.as_ref().expect("open").commit_block(&mut w, block.number, &changes).map_err(err)?;
                w.commit().map_err(err)?;
                root
            } else {
                let changes = hashed_changes(block.revert.iter().chain(&block.ops));
                if mode == Run::CrashHeal {
                    let mut w = slot.as_ref().expect("open").write().map_err(err)?;
                    slot.as_ref().expect("open").commit_hashed(&mut w, block.number, &changes).map_err(err)?;
                    w.crash_before_mdbx(true).map_err(err)?;
                    reopen(&mut slot, dir)?;
                }
                let mut w = slot.as_ref().expect("open").write().map_err(err)?;
                let root = slot.as_ref().expect("open").commit_hashed(&mut w, block.number, &changes).map_err(err)?;
                w.commit().map_err(err)?;
                root
            };
            tip = block.number;
            let reopen_now = match mode {
                Run::Primary | Run::CrashHeal => false,
                // Every block for short requests, every fourth for long ones (opening is not free).
                Run::Reopen => blocks.len() <= 32 || index % 4 == 3,
                Run::TornTail => {
                    tear_static_files(dir, index as u64).map_err(err)?;
                    true
                }
            };
            if reopen_now {
                reopen(&mut slot, dir)?;
                if slot.as_ref().expect("open").tip().map_err(err)? != Some(tip) {
                    return Err(format!("reopened at {:?}, committed {tip}", slot.as_ref().expect("open").tip().map_err(err)?));
                }
            }
            let view = slot.as_ref().expect("open").view(tip, Vec::new()).map_err(err)?;
            let stored_root = view.state_root(None).map_err(err)?;
            if stored_root != root {
                return Err(format!("block {tip}: commit returned {root}, the stored state has root {stored_root}"));
            }
            block_result(&view, block, root, preimages).map_err(err)
        }));
        let result = match step {
            Ok(result) => result,
            Err(panic) => Err(format!("panicked: {}", panic_message(&*panic))),
        };
        match result {
            Ok(value) => results.push(value),
            Err(error) => {
                // As base/base's adapter: the failing block reports the error, later blocks
                // report nothing.
                results.push(json!({ "number": format!("{:#x}", block.number), "error": error }));
                break;
            }
        }
    }
    results.resize(blocks.len(), json!({}));
    Ok(results)
}

/// Closes and reopens the datadir.
fn reopen(slot: &mut Option<StateDb>, dir: &Path) -> Result<(), String> {
    *slot = None;
    *slot = Some(StateDb::open(dir).map_err(err)?);
    Ok(())
}

/// The text of a caught panic.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_owned())
}

/// The `overlay` and `history` variants (preimages only).
fn run_views(dir: &Path, blocks: &[Block]) -> Result<(Vec<Value>, Vec<Value>), String> {
    let db = StateDb::open(dir).map_err(err)?;
    let mut w = db.write().map_err(err)?;
    db.commit_block(&mut w, 0, &BlockChanges::default()).map_err(err)?;
    w.commit().map_err(err)?;
    let mut overlay = Vec::with_capacity(blocks.len());
    let mut canonical: Vec<usize> = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if block.unwind > 0 {
            db.unwind_to(block.number - 1).map_err(err)?;
            canonical.truncate(canonical.len() - block.unwind as usize);
        }
        let changes = block_changes(&block.ops);
        let view = db.view(block.number - 1, vec![changes.clone()]).map_err(err)?;
        let root = view.state_root(None).map_err(err)?;
        let mut storage_roots = serde_json::Map::new();
        for key in &block.named {
            if view.account_hashed(*key).map_err(err)?.is_some() {
                storage_roots.insert(format!("{key:#x}"), json!(format!("{:#x}", view.storage_root_hashed(*key).map_err(err)?)));
            }
        }
        drop(view);
        overlay.push(json!({ "number": format!("{:#x}", block.number), "stateRoot": format!("{root:#x}"), "storageRoots": storage_roots }));
        let mut w = db.write().map_err(err)?;
        db.commit_block(&mut w, block.number, &changes).map_err(err)?;
        w.commit().map_err(err)?;
        canonical.push(index);
    }
    let mut history = vec![json!({}); blocks.len()];
    for index in canonical {
        let block = &blocks[index];
        let view = db.view(block.number, Vec::new()).map_err(err)?;
        let root = view.state_root(None).map_err(err)?;
        history[index] = block_result(&view, block, root, true).map_err(err)?;
    }
    Ok((overlay, history))
}

fn variant(dir: PathBuf, f: impl FnOnce(&Path) -> Result<Vec<Value>, String>, count: usize) -> Value {
    let result = catch_unwind(AssertUnwindSafe(|| f(&dir)))
        .unwrap_or_else(|panic| Err(format!("panicked: {}", panic_message(&*panic))));
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(results) => Value::Array(results),
        Err(error) => Value::Array((0..count).map(|_| json!({ "error": error.clone() })).collect()),
    }
}

/// Answers one `trie` mode request with its response object.
pub fn handle(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let failure = |error: String| json!({ "id": id.clone(), "ok": false, "error": error });
    let Some(trie) = request.get("trie") else { return failure("missing trie".into()) };
    let Some(dir) = trie.get("dir").and_then(Value::as_str) else { return failure("missing trie.dir".into()) };
    let dir = PathBuf::from(dir);
    let (blocks, preimages) = match parse_blocks(trie) {
        Ok(parsed) => parsed,
        Err(error) => return failure(error),
    };
    let count = blocks.len();
    let primary = catch_unwind(AssertUnwindSafe(|| run(&dir.join("primary"), &blocks, preimages, Run::Primary)))
        .unwrap_or_else(|panic| Err(format!("panicked: {}", panic_message(&*panic))));
    let _ = std::fs::remove_dir_all(dir.join("primary"));
    let primary = match primary {
        Ok(results) => results,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            return failure(error);
        }
    };
    let mut variants = serde_json::Map::new();
    variants.insert("reopen".into(), variant(dir.join("reopen"), |d| run(d, &blocks, preimages, Run::Reopen), count));
    variants.insert("crash-heal".into(), variant(dir.join("crash"), |d| run(d, &blocks, preimages, Run::CrashHeal), count));
    if preimages {
        variants.insert("torn-tail".into(), variant(dir.join("torn"), |d| run(d, &blocks, true, Run::TornTail), count));
        let views = catch_unwind(AssertUnwindSafe(|| run_views(&dir.join("views"), &blocks)))
            .unwrap_or_else(|panic| Err(format!("panicked: {}", panic_message(&*panic))));
        let _ = std::fs::remove_dir_all(dir.join("views"));
        match views {
            Ok((overlay, history)) => {
                variants.insert("overlay".into(), Value::Array(overlay));
                variants.insert("history".into(), Value::Array(history));
            }
            Err(error) => {
                variants.insert("history".into(), Value::Array((0..count).map(|_| json!({ "error": error.clone() })).collect()));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    json!({ "id": id, "ok": true, "blocks": primary, "variants": variants })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> (Address, B256) {
        let address = Address::repeat_byte(n);
        (address, keccak256(address))
    }

    #[test]
    fn answers_a_request_with_preimages_and_unwind() {
        let dir = std::env::temp_dir().join(format!("tiger-trie-mode-{}", std::process::id()));
        let (a, ka) = key(1);
        let (b, kb) = key(2);
        let slot_key = B256::with_last_byte(5);
        let slot = keccak256(slot_key);
        let account = |k: B256, addr: Address, nonce: u64| {
            json!({ "op": "account", "key": k, "address": addr, "nonce": format!("{nonce:#x}"), "balance": "0x10",
                    "codeHash": format!("{:#x}", alloy_trie::KECCAK_EMPTY) })
        };
        let store = json!({ "op": "storage", "key": ka, "address": a, "slot": slot, "slotKey": slot_key,
                            "value": format!("{:#x}", B256::with_last_byte(9)) });
        let target = json!({ "key": ka, "address": a, "slots": [slot], "slotKeys": [slot_key] });
        let request = json!({ "id": "t1", "mode": "trie", "trie": { "dir": dir, "blocks": [
            { "number": "0x1", "ops": [account(ka, a, 1), store], "proofs": [target] },
            { "number": "0x2", "ops": [account(kb, b, 1), { "op": "wipe", "key": ka, "address": a }], "proofs": [target] },
            { "number": "0x2", "unwind": 1, "revert": [{ "op": "delete", "key": kb, "address": b }],
              "ops": [{ "op": "delete", "key": ka, "address": a }], "proofs": [target] },
        ]}});
        let response = handle(&request);
        assert_eq!(response["ok"], true, "{response}");
        let blocks = response["blocks"].as_array().unwrap();
        assert_eq!(blocks[2]["stateRoot"], format!("{:#x}", alloy_trie::EMPTY_ROOT_HASH));
        assert_eq!(blocks[0]["proofs"][0]["storage"][0]["value"], format!("{:#x}", B256::with_last_byte(9)));
        assert_eq!(blocks[1]["proofs"][0]["storage"][0]["value"], format!("{:#x}", B256::ZERO));
        for (name, results) in response["variants"].as_object().unwrap() {
            for (i, result) in results.as_array().unwrap().iter().enumerate() {
                assert!(result.get("error").is_none(), "{name}: {result}");
                if result == &json!({}) {
                    continue;
                }
                assert_eq!(result["stateRoot"], blocks[i]["stateRoot"], "{name} block {i}");
            }
        }
        assert_eq!(response["variants"]["history"][1], json!({}), "the unwound block has no history");
        assert!(!dir.exists());
    }
}
