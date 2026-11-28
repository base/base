//! A recorded chain: every Engine API call base-reth accepted while the diff-fuzz state generator
//! built a tree of branches, the blocks those calls carry, and base-reth's answers for the final
//! canonical chain. Chaos runs replay it into tiger; base-reth is the oracle.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use base_diff_fuzz::{
    Adapter, AdapterSpec, Era, GeneratorOptions, Lag, NodeClient, NodeProcess, Rng, StateOp,
    StateSession,
};
use eyre::{WrapErr, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What one recorded Engine API call does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StepKind {
    /// Submits a valid block.
    Payload {
        /// The block.
        hash: B256,
    },
    /// Submits a block every client must refuse (a wrong state root).
    Invalid {
        /// The block's parent.
        parent: B256,
    },
    /// Moves the forkchoice.
    Forkchoice {
        /// Head.
        head: B256,
        /// Safe block.
        safe: B256,
        /// Finalized block.
        finalized: B256,
    },
}

/// One recorded Engine API call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Engine API method.
    pub method: String,
    /// Its parameters.
    pub params: Value,
    /// What it does.
    pub kind: StepKind,
}

/// One valid block of the recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockInfo {
    /// Number.
    pub number: u64,
    /// Hash.
    pub hash: B256,
    /// Parent hash.
    pub parent: B256,
    /// State root.
    pub state_root: B256,
    /// Index of the step that submits it (`None` for genesis).
    pub step: Option<usize>,
    /// Transaction hashes in order.
    pub txs: Vec<B256>,
    /// Accounts and slots the block changed.
    pub touched: Vec<(Address, Vec<B256>)>,
    /// Whether the block deleted or wiped an account that held storage (an EIP-161 deletion or
    /// a `SELFDESTRUCT`). base-reth 2b20e42's storage-v2 changesets leave the wiped slots out
    /// (finding B1), so on a datadir base-reth wrote, state at every block below this one is
    /// wrong in base-reth's historical reads and proofs, and tiger reading that datadir agrees.
    #[serde(default)]
    pub wipes_storage: bool,
}

/// base-reth's answers for one block of the final canonical chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleBlock {
    /// `eth_getBlockByNumber(n, false)`.
    pub block: Value,
    /// `eth_getBlockReceipts(n)`.
    pub receipts: Value,
}

/// A base-reth datadir copied after a clean stop partway through recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Steps already applied to the snapshot's datadir.
    pub steps: usize,
    /// Its head.
    pub head: B256,
    /// Its safe block.
    pub safe: B256,
    /// Its finalized block.
    pub finalized: B256,
}

/// A recorded chain (`chain.json` in a recording directory, next to `genesis.json`, the
/// pristine oracle datadir `oracle-datadir/`, and the snapshot datadir `snapshot-datadir/`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    /// Chain seed.
    pub seed: u64,
    /// Genesis hash.
    pub genesis: B256,
    /// Engine API calls in order.
    pub steps: Vec<Step>,
    /// Every valid block, by hash.
    pub blocks: HashMap<B256, BlockInfo>,
    /// base-reth's answers per canonical height of the final chain.
    pub oracle: BTreeMap<u64, OracleBlock>,
    /// The mid-recording base-reth datadir.
    pub snapshot: Option<Snapshot>,
}

impl Recording {
    /// The recording file's name in its directory.
    pub const FILE: &str = "chain.json";

    /// Loads `<dir>/chain.json`.
    pub fn load(dir: &Path) -> eyre::Result<Self> {
        let path = dir.join(Self::FILE);
        let text =
            fs::read_to_string(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
        let mut de = serde_json::Deserializer::from_str(&text);
        de.disable_recursion_limit();
        Ok(Self::deserialize(&mut de)?)
    }

    /// The block `hash`.
    pub fn block(&self, hash: &B256) -> Option<&BlockInfo> {
        self.blocks.get(hash)
    }

    /// The ancestor of `hash` at `number` (itself at its own number), if `number` is not above it.
    pub fn ancestor_at(&self, hash: B256, number: u64) -> Option<B256> {
        let mut at = self.blocks.get(&hash)?;
        if number > at.number {
            return None;
        }
        while at.number > number {
            at = self.blocks.get(&at.parent)?;
        }
        Some(at.hash)
    }

    /// Whether `block` is on the chain ending at `head`.
    pub fn is_canonical_under(&self, block: B256, head: B256) -> bool {
        self.blocks.get(&block).is_some_and(|b| self.ancestor_at(head, b.number) == Some(block))
    }

    /// The final forkchoice head.
    pub fn final_head(&self) -> B256 {
        self.steps
            .iter()
            .rev()
            .find_map(|s| match s.kind {
                StepKind::Forkchoice { head, .. } => Some(head),
                _ => None,
            })
            .unwrap_or(self.genesis)
    }

    /// Records a chain: base-reth (node `base` through `launch`) follows the state generator's
    /// chain of seed `seed` through extensions, forks, branch switches and invalid blocks until
    /// the canonical chain is `blocks` long. Writes `out/chain.json`, `out/genesis.json`,
    /// `out/oracle-datadir/` and `out/snapshot-datadir/`.
    pub fn record(
        seed: u64,
        blocks: u64,
        adapter: &str,
        launch: &Path,
        root: &Path,
        base: &str,
        out: &Path,
    ) -> eyre::Result<Self> {
        fs::create_dir_all(root)?;
        fs::create_dir_all(out)?;
        let jwt = root.join("jwt.hex");
        if !jwt.exists() {
            fs::write(&jwt, alloy_primitives::hex::encode(B256::random()))?;
        }
        let node = NodeClient::parse(base, &jwt)?;
        let proc = NodeProcess::new(node.name.clone(), launch.to_path_buf(), root.to_path_buf());
        let adapter = Adapter::new(AdapterSpec::parse(adapter)?, Duration::from_secs(300));
        let options = GeneratorOptions {
            eras: vec![Era::Isthmus, Era::Jovian, Era::Azul, Era::Beryl, Era::Cobalt, Era::Denim],
            max_nonce_deposit_creates: false,
            state_stress: true,
            ..GeneratorOptions::default()
        };
        let mut session = StateSession::start(
            seed,
            options,
            adapter,
            vec![node.clone()],
            vec![proc],
            Lag { safe: 4, finalized: 48 },
            40,
        )?;
        let mut rng = Rng::new(&[seed, 0xc4a0_5eed]);
        let mut touched: HashMap<B256, Vec<(Address, Vec<B256>)>> = HashMap::new();
        let mut wipes: HashSet<B256> = HashSet::new();
        let mut snapshot = None;
        let mut ops = 0u64;
        while session.height() < blocks {
            let op = match StateOp::random(&mut rng, 40) {
                op @ (StateOp::Extend { .. }
                | StateOp::Fork { .. }
                | StateOp::Switch { .. }
                | StateOp::BadRoot { .. }) => op,
                _ => continue,
            };
            let divergences = session.run(&op)?;
            ops += 1;
            ensure!(
                divergences.is_empty(),
                "base-reth diverged from the generator at {op:?}: {divergences:?}"
            );
            for block in session.tree.blocks.values() {
                if touched.contains_key(&block.hash) {
                    continue;
                }
                touched.insert(
                    block.hash,
                    block
                        .touched
                        .iter()
                        .map(|(a, slots)| (*a, slots.iter().copied().collect()))
                        .collect(),
                );
                let parent =
                    session.tree.blocks.get(&block.parent).and_then(|p| p.generator.as_ref());
                if let (Some(parent), Some(child)) = (parent, block.generator.as_ref()) {
                    // An account that held storage is gone. From Cancun on (every era these chains
                    // run), SELFDESTRUCT deletes only accounts created in the same transaction, so
                    // the wipes are EIP-161 deletions of storage-holding genesis allocations.
                    let wiped = block.touched.keys().any(|address| {
                        parent.state().account(address).is_some_and(|a| !a.storage.is_empty())
                            && child.state().account(address).is_none()
                    });
                    if wiped {
                        wipes.insert(block.hash);
                    }
                }
            }
            if snapshot.is_none() && session.height() >= blocks / 2 {
                let proc = &mut session.procs[0];
                ensure!(proc.stop(false), "base-reth did not stop cleanly for the snapshot");
                copy_dir(&proc.datadir(), &out.join("snapshot-datadir"))?;
                proc.spawn(false)?;
                proc.wait_ready(&node)?;
                let tree = &session.tree;
                let safe = tree.ancestor(tree.head, session.lag.safe);
                snapshot = Some(Snapshot {
                    steps: engine_steps(&session.trace).count(),
                    head: tree.head,
                    safe,
                    finalized: tree.finalized,
                });
            }
        }
        eprintln!("recorded {ops} operations, height {}", session.height());
        let genesis = session.tree.genesis;
        let genesis_block = session.tree.block(&genesis).clone();
        let mut recording = Self {
            seed,
            genesis,
            steps: Vec::new(),
            blocks: HashMap::new(),
            oracle: BTreeMap::new(),
            snapshot,
        };
        recording.blocks.insert(
            genesis,
            BlockInfo {
                number: genesis_block.number,
                hash: genesis,
                parent: B256::ZERO,
                state_root: genesis_block.state_root,
                step: None,
                txs: Vec::new(),
                touched: Vec::new(),
                wipes_storage: false,
            },
        );
        for entry in engine_steps(&session.trace) {
            let method = entry["method"].as_str().unwrap_or_default().to_owned();
            let params = entry["params"].clone();
            let index = recording.steps.len();
            let kind = if method.starts_with("engine_forkchoiceUpdated") {
                let state = &params[0];
                StepKind::Forkchoice {
                    head: serde_json::from_value(state["headBlockHash"].clone())?,
                    safe: serde_json::from_value(state["safeBlockHash"].clone())?,
                    finalized: serde_json::from_value(state["finalizedBlockHash"].clone())?,
                }
            } else {
                let payload = &params[0];
                let payload = payload.get("executionPayload").unwrap_or(payload);
                let inner = Self::payload_fields(payload);
                let hash: B256 = serde_json::from_value(inner["blockHash"].clone())?;
                let parent: B256 = serde_json::from_value(inner["parentHash"].clone())?;
                if entry.get("expect").and_then(Value::as_str) == Some("INVALID") {
                    StepKind::Invalid { parent }
                } else {
                    let txs: Vec<Bytes> = serde_json::from_value(inner["transactions"].clone())?;
                    let number = u64::from_str_radix(
                        inner["blockNumber"].as_str().unwrap_or("0x0").trim_start_matches("0x"),
                        16,
                    )?;
                    recording.blocks.entry(hash).or_insert(BlockInfo {
                        number,
                        hash,
                        parent,
                        state_root: serde_json::from_value(inner["stateRoot"].clone())?,
                        step: Some(index),
                        txs: txs.iter().map(keccak256).collect(),
                        touched: touched.get(&hash).cloned().unwrap_or_default(),
                        wipes_storage: wipes.contains(&hash),
                    });
                    StepKind::Payload { hash }
                }
            };
            recording.steps.push(Step { method, params, kind });
        }
        let head = recording.final_head();
        let head_number = recording.blocks[&head].number;
        for number in 0..=head_number {
            let tag = format!("{number:#x}");
            let block = node.result("eth_getBlockByNumber", &json!([tag, false]))?;
            ensure!(
                block["hash"] == json!(recording.ancestor_at(head, number)),
                "base-reth's block {number} is not on the recorded head's chain"
            );
            let receipts = node.result("eth_getBlockReceipts", &json!([tag]))?;
            recording.oracle.insert(number, OracleBlock { block, receipts });
        }
        let proc = &mut session.procs[0];
        ensure!(proc.stop(false), "base-reth did not stop cleanly");
        copy_dir(&proc.datadir(), &out.join("oracle-datadir"))?;
        fs::copy(root.join("genesis.json"), out.join("genesis.json"))?;
        fs::write(out.join(Self::FILE), serde_json::to_vec(&recording)?)?;
        Ok(recording)
    }

    /// The execution payload's own fields (V3 and later nest V1 and V2 fields).
    fn payload_fields(payload: &Value) -> &Value {
        let mut at = payload;
        while at.get("blockHash").is_none() {
            match at.get("payloadInner") {
                Some(inner) => at = inner,
                None => return payload,
            }
        }
        at
    }

    /// Every block's touched keys as `(address, slots)` pairs over the blocks on the chain ending
    /// at `hash`, nearest first, at most `limit` pairs.
    pub fn keys_near(&self, hash: B256, limit: usize) -> Vec<(Address, Vec<B256>)> {
        let mut keys = Vec::new();
        let mut seen = BTreeSet::new();
        let mut at = self.blocks.get(&hash);
        while let Some(block) = at {
            for (address, slots) in &block.touched {
                if seen.insert(*address) {
                    keys.push((*address, slots.iter().take(4).copied().collect()));
                    if keys.len() >= limit {
                        return keys;
                    }
                }
            }
            at = self.blocks.get(&block.parent);
        }
        keys
    }
}

/// The trace's Engine API calls, without restarts.
fn engine_steps(trace: &[Value]) -> impl Iterator<Item = &Value> {
    trace.iter().filter(|e| e["kind"] == "engine" && e.get("node").is_none())
}

/// Copies a directory tree (replacing `to`).
pub fn copy_dir(from: &Path, to: &Path) -> eyre::Result<()> {
    if to.exists() {
        fs::remove_dir_all(to)?;
    }
    let status = std::process::Command::new("cp").arg("-a").arg(from).arg(to).status()?;
    if !status.success() {
        bail!("cp -a {} {} failed", from.display(), to.display());
    }
    Ok(())
}

/// The recording directory's snapshot datadir.
pub(crate) fn snapshot_datadir(dir: &Path) -> PathBuf {
    dir.join("snapshot-datadir")
}

/// Parses a hex quantity.
pub(crate) fn quantity(value: &Value) -> Option<u64> {
    u64::from_str_radix(value.as_str()?.strip_prefix("0x")?, 16).ok()
}

/// Parses a hash field.
pub(crate) fn hash(value: &Value) -> Option<B256> {
    serde_json::from_value(value.clone()).ok()
}
