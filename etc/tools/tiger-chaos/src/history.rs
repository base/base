//! Jepsen-style history checking of tiger's JSON-RPC reads during chaos.
//!
//! The driver logs every forkchoice update it sends (send and acknowledgement times) and every
//! process restart into a [`Timeline`]. Reader threads read the head, canonical blocks, proofs
//! and state concurrently, and each read is checked against the forkchoice states that could
//! have been current while it ran: the last state acknowledged before the read started, plus
//! every update sent before it finished and not acknowledged before it started. A read showing
//! any other head, a state root no recorded block has, a proof that does not verify, or a
//! canonical block that went missing is a violation. Reads are grouped by process lifetime
//! (epoch): after a crash the node may legitimately reopen at an earlier persisted block.

use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_trie::{EMPTY_ROOT_HASH, Nibbles, TrieAccount, proof::verify_proof};
use base_diff_fuzz::{NodeClient, Reply, Rng};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Recording, StepKind, recording};

/// One forkchoice update the driver sent.
#[derive(Debug, Clone, Serialize)]
pub struct Fcu {
    /// Process lifetime it was sent in.
    pub epoch: u64,
    /// Seconds since the run started, when sent.
    pub sent: f64,
    /// When acknowledged `VALID`; `None` when it failed or got no answer.
    pub acked: Option<f64>,
    /// The head the node keeps after it (an update to an ancestor of the head keeps the head).
    pub head: B256,
    /// Safe block.
    pub safe: B256,
    /// Finalized block.
    pub finalized: B256,
}

/// One process lifetime.
#[derive(Debug, Clone, Serialize)]
pub struct Epoch {
    /// The head the node reopened at, read before readers start.
    pub recovered: B256,
    /// When readers were let in.
    pub opened: f64,
}

/// What the driver did, in time order, shared with the readers.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Process lifetimes, by index.
    pub epochs: Vec<Epoch>,
    /// Forkchoice updates in send order.
    pub fcus: Vec<Fcu>,
}

/// A read's window: the forkchoice states it may observe.
#[derive(Debug, Clone, Default)]
pub struct Allowed {
    /// Possible heads.
    pub heads: Vec<B256>,
    /// Possible safe blocks (empty when no update was acknowledged in the epoch yet).
    pub safe: Vec<B256>,
    /// Possible finalized blocks (as `safe`).
    pub finalized: Vec<B256>,
}

impl Timeline {
    /// The forkchoice states a read in `epoch` over `[invoked, completed]` may observe.
    pub fn allowed(&self, epoch: u64, invoked: f64, completed: f64) -> Allowed {
        let Some(info) = self.epochs.get(epoch as usize) else { return Allowed::default() };
        let fcus: Vec<&Fcu> = self.fcus.iter().filter(|f| f.epoch == epoch).collect();
        let last_acked = fcus.iter().rposition(|f| f.acked.is_some_and(|a| a < invoked));
        let mut allowed = Allowed::default();
        match last_acked {
            Some(i) => {
                allowed.heads.push(fcus[i].head);
                allowed.safe.push(fcus[i].safe);
                allowed.finalized.push(fcus[i].finalized);
            }
            None => allowed.heads.push(info.recovered),
        }
        let from = last_acked.map_or(0, |i| i + 1);
        for fcu in &fcus[from..] {
            if fcu.sent < completed {
                allowed.heads.push(fcu.head);
                allowed.safe.push(fcu.safe);
                allowed.finalized.push(fcu.finalized);
            }
        }
        // Before the epoch's first acknowledged update the tags are unknown: skip them.
        if last_acked.is_none() {
            allowed.safe.clear();
            allowed.finalized.clear();
        }
        allowed
    }
}

/// A violation found by a reader or the driver.
#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    /// What was violated, such as `head`, `proof-root`, or `recovered-head`.
    pub kind: String,
    /// Epoch.
    pub epoch: u64,
    /// Seconds since the start, when it was seen.
    pub at: f64,
    /// The request.
    pub request: Value,
    /// What was allowed or expected.
    pub expected: Value,
    /// What the node answered.
    pub got: Value,
}

/// State shared by the driver and the readers.
#[derive(Debug)]
pub struct Shared {
    /// Run start.
    pub start: Instant,
    /// The recording.
    pub recording: Arc<Recording>,
    /// The timeline.
    pub timeline: Mutex<Timeline>,
    /// Whether readers may read (the node is up and its epoch registered).
    pub open: AtomicBool,
    /// The current epoch.
    pub epoch: AtomicU64,
    /// Steps the driver has sent: readers pick blocks among them.
    pub progress: AtomicUsize,
    /// Whether readers should exit.
    pub stop: AtomicBool,
    /// Violations.
    pub violations: Mutex<Vec<Violation>>,
    /// Blocks whose historical state base-reth wrote wrong (finding B1: a later block wiped a
    /// storage-holding account and base-reth's changesets left the wiped slots out; see
    /// [`crate::ChaosRun::base_reth_quirks`]). In a run on base-reth's datadir, tiger reads
    /// the same history, so their roots are not checked.
    pub quirks: std::sync::RwLock<HashSet<B256>>,
    /// Read counts by kind.
    pub reads: Mutex<BTreeMap<String, u64>>,
    /// Reads that failed on transport or a JSON-RPC error, by kind (not violations).
    pub failed: Mutex<BTreeMap<String, u64>>,
}

impl Shared {
    /// Seconds since the start.
    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Records a violation.
    pub fn violation(&self, kind: &str, epoch: u64, request: Value, expected: Value, got: Value) {
        let violation =
            Violation { kind: kind.to_owned(), epoch, at: self.now(), request, expected, got };
        eprintln!("VIOLATION {}", serde_json::to_string(&violation).unwrap_or_default());
        self.violations.lock().expect("violations").push(violation);
    }

    fn count(&self, map: &Mutex<BTreeMap<String, u64>>, kind: &str) {
        *map.lock().expect("counts").entry(kind.to_owned()).or_default() += 1;
    }
}

/// One reader thread's loop.
#[derive(Debug)]
pub struct Reader {
    shared: Arc<Shared>,
    node: NodeClient,
    rng: Rng,
}

/// What a read returned.
enum Got {
    Value(Value),
    /// A JSON-RPC error.
    Error(Value),
    /// No answer (the node is down).
    Transport,
}

impl Reader {
    /// Starts `count` reader threads.
    pub fn spawn(
        shared: &Arc<Shared>,
        node: &NodeClient,
        count: usize,
        seed: u64,
    ) -> Vec<thread::JoinHandle<()>> {
        (0..count)
            .map(|i| {
                let mut reader = Self {
                    shared: Arc::clone(shared),
                    node: node.clone(),
                    rng: Rng::new(&[seed, 0x4ead, i as u64]),
                };
                thread::spawn(move || reader.run())
            })
            .collect()
    }

    fn run(&mut self) {
        while !self.shared.stop.load(Ordering::SeqCst) {
            if !self.shared.open.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            self.read_once();
        }
    }

    /// A random sent block.
    fn sent_block(&mut self) -> Option<B256> {
        let rec = &self.shared.recording;
        let progress = self.shared.progress.load(Ordering::SeqCst).min(rec.steps.len());
        for _ in 0..8 {
            if progress == 0 {
                return None;
            }
            let step = &rec.steps[self.rng.index(progress)];
            if let StepKind::Payload { hash } = step.kind {
                return Some(hash);
            }
        }
        None
    }

    fn call(&self, method: &str, params: &Value) -> (f64, f64, u64, Got) {
        let epoch = self.shared.epoch.load(Ordering::SeqCst);
        let invoked = self.shared.now();
        let reply = self.node.call(method, params);
        let completed = self.shared.now();
        let got = match reply {
            Reply::Body(mut body) => body.get_mut("error").map(Value::take).map_or_else(
                || Got::Value(body.get_mut("result").map(Value::take).unwrap_or_default()),
                Got::Error,
            ),
            Reply::Transport(_) => Got::Transport,
        };
        (invoked, completed, epoch, got)
    }

    fn read_once(&mut self) {
        let kind = self.rng.weighted(&[18, 14, 18, 14, 10, 6, 6, 8, 6, 6]);
        match kind {
            0 => self.read_head(),
            1 => self.read_by_number(),
            2 => self.read_proof(Some("latest")),
            3 => self.read_proof(None),
            4 => self.read_at_hash(),
            5 => self.read_tag("safe"),
            6 => self.read_tag("finalized"),
            7 => self.read_receipts(),
            8 => self.read_tx(),
            _ => self.read_logs(),
        }
    }

    /// Allowed states for a read, re-taken until the epoch did not change under it.
    fn allowed(&self, epoch: u64, invoked: f64, completed: f64) -> Allowed {
        self.shared.timeline.lock().expect("timeline").allowed(epoch, invoked, completed)
    }

    fn failed(&self, kind: &str, got: &Got, request: &Value, epoch: u64) {
        match got {
            Got::Transport => self.shared.count(&self.shared.failed, &format!("{kind}:transport")),
            Got::Error(error) => {
                self.shared.count(&self.shared.failed, &format!("{kind}:error"));
                // A JSON-RPC error on a read the node can always answer is a violation unless
                // a fault window is open (the driver records those separately).
                if self.shared.open.load(Ordering::SeqCst) && !crate::chaos::fault_window_open() {
                    self.shared.violation(
                        &format!("{kind}-error"),
                        epoch,
                        request.clone(),
                        json!("an answer"),
                        error.clone(),
                    );
                }
            }
            Got::Value(_) => {}
        }
    }

    /// Checks a block object against the recording; returns its hash.
    fn known_block(&self, kind: &str, epoch: u64, request: &Value, block: &Value) -> Option<B256> {
        let hash = recording::hash(&block["hash"])?;
        let rec = &self.shared.recording;
        let ok = rec.block(&hash).is_some_and(|b| {
            recording::quantity(&block["number"]) == Some(b.number)
                && recording::hash(&block["stateRoot"]) == Some(b.state_root)
                && recording::hash(&block["parentHash"]) == Some(b.parent)
        });
        if !ok {
            self.shared.violation(
                &format!("{kind}-unknown-block"),
                epoch,
                request.clone(),
                json!("a recorded block"),
                block.clone(),
            );
            return None;
        }
        Some(hash)
    }

    fn read_head(&self) {
        let request = json!(["latest", false]);
        let (inv, cmp, epoch, got) = self.call("eth_getBlockByNumber", &request);
        self.shared.count(&self.shared.reads, "head");
        let Got::Value(block) = &got else { return self.failed("head", &got, &request, epoch) };
        let Some(hash) = self.known_block("head", epoch, &request, block) else { return };
        let allowed = self.allowed(epoch, inv, cmp);
        if !allowed.heads.contains(&hash) {
            self.shared.violation(
                "head",
                epoch,
                request,
                json!(allowed.heads),
                json!({ "hash": hash, "number": block["number"], "window": [inv, cmp] }),
            );
        }
    }

    fn read_tag(&self, tag: &str) {
        let request = json!([tag, false]);
        let (inv, cmp, epoch, got) = self.call("eth_getBlockByNumber", &request);
        self.shared.count(&self.shared.reads, tag);
        let Got::Value(block) = &got else { return self.failed(tag, &got, &request, epoch) };
        let allowed = self.allowed(epoch, inv, cmp);
        let set = if tag == "safe" { &allowed.safe } else { &allowed.finalized };
        if set.is_empty() {
            return;
        }
        if block.is_null() {
            // reth answers null for an unset tag; every update the harness sends sets both.
            self.shared.violation(tag, epoch, request, json!(set), Value::Null);
            return;
        }
        let Some(hash) = self.known_block(tag, epoch, &request, block) else { return };
        if !set.contains(&hash) {
            self.shared.violation(
                tag,
                epoch,
                request,
                json!(set),
                json!({ "hash": hash, "window": [inv, cmp] }),
            );
        }
    }

    fn read_by_number(&mut self) {
        let Some(target) = self.sent_block() else { return };
        let number = self.shared.recording.blocks[&target].number;
        let request = json!([format!("{number:#x}"), false]);
        let (inv, cmp, epoch, got) = self.call("eth_getBlockByNumber", &request);
        self.shared.count(&self.shared.reads, "byNumber");
        let Got::Value(block) = &got else { return self.failed("byNumber", &got, &request, epoch) };
        let allowed = self.allowed(epoch, inv, cmp);
        let rec = &self.shared.recording;
        let expected: Vec<Option<B256>> =
            allowed.heads.iter().map(|h| rec.ancestor_at(*h, number)).collect();
        let got_hash = if block.is_null() {
            None
        } else {
            match self.known_block("byNumber", epoch, &request, block) {
                Some(hash) => Some(hash),
                None => return,
            }
        };
        if !expected.contains(&got_hash) {
            self.shared.violation(
                "byNumber",
                epoch,
                request,
                json!(expected),
                json!({ "hash": got_hash, "window": [inv, cmp] }),
            );
        }
    }

    /// `eth_getProof` at `latest` (`tag`) or at a sent block's number.
    fn read_proof(&mut self, tag: Option<&str>) {
        let rec = Arc::clone(&self.shared.recording);
        let Some(target) = self.sent_block() else { return };
        let keys = rec.keys_near(target, 24);
        let (address, slots) = if keys.is_empty() {
            (Address::ZERO, Vec::new())
        } else {
            keys[self.rng.index(keys.len())].clone()
        };
        let number = rec.blocks[&target].number;
        let at = tag.map_or_else(|| json!(format!("{number:#x}")), |t| json!(t));
        let request = json!([address, slots, at]);
        let (inv, cmp, epoch, got) = self.call("eth_getProof", &request);
        let kind = if tag.is_some() { "proofLatest" } else { "proofNumber" };
        self.shared.count(&self.shared.reads, kind);
        let allowed = self.allowed(epoch, inv, cmp);
        let expected: Vec<Option<B256>> = allowed
            .heads
            .iter()
            .map(|h| if tag.is_some() { Some(*h) } else { rec.ancestor_at(*h, number) })
            .map(|b| b.map(|b| rec.blocks[&b].state_root))
            .collect();
        let proof = match &got {
            Got::Value(proof) => proof,
            // A block that some allowed head does not have at that number (a reorg during the
            // read) may be refused.
            Got::Error(_) if tag.is_none() && expected.iter().any(|e| *e != expected[0]) => {
                self.shared.count(&self.shared.failed, "proofNumber:reorged");
                return;
            }
            Got::Error(_) if tag.is_none() && expected.contains(&None) => return,
            _ => return self.failed(kind, &got, &request, epoch),
        };
        let root = match proof_root(proof) {
            Ok(root) => root,
            Err(error) => {
                self.shared.violation(
                    &format!("{kind}-malformed"),
                    epoch,
                    request,
                    json!("a proof"),
                    json!({ "error": error, "proof": proof }),
                );
                return;
            }
        };
        let quirk = allowed.heads.iter().any(|h| {
            let at = if tag.is_some() { Some(*h) } else { rec.ancestor_at(*h, number) };
            at.is_some_and(|b| self.shared.quirks.read().expect("quirks").contains(&b))
        });
        if quirk {
            self.shared.count(&self.shared.reads, "quirkSkipped");
            return;
        }
        if !expected.contains(&Some(root)) {
            let known =
                rec.blocks.values().find(|b| b.state_root == root).map(|b| (b.number, b.hash));
            self.shared.violation(
                &format!("{kind}-root"),
                epoch,
                request,
                json!(expected),
                json!({ "root": root, "rootOfBlock": known, "window": [inv, cmp] }),
            );
            return;
        }
        if let Err(error) = verify_account_proof(address, &slots, root, proof) {
            self.shared.violation(
                &format!("{kind}-verify"),
                epoch,
                request,
                json!(root),
                json!({ "error": error, "proof": proof }),
            );
        }
    }

    /// Proof and plain state reads at a sent block's hash: the proof root must be that block's,
    /// the plain reads must agree with the proof, and a block canonical under every allowed head
    /// must be served.
    fn read_at_hash(&mut self) {
        let rec = Arc::clone(&self.shared.recording);
        let Some(target) = self.sent_block() else { return };
        let block = &rec.blocks[&target];
        let keys = rec.keys_near(target, 24);
        let (address, slots) = if keys.is_empty() {
            (Address::ZERO, Vec::new())
        } else {
            keys[self.rng.index(keys.len())].clone()
        };
        let at = json!({ "blockHash": target });
        let request = json!([address, slots, at]);
        let (inv, cmp, epoch, got) = self.call("eth_getProof", &request);
        self.shared.count(&self.shared.reads, "proofHash");
        let must_serve = |epoch: u64, inv: f64, cmp: f64| {
            let allowed = self.allowed(epoch, inv, cmp);
            allowed.heads.iter().all(|h| rec.is_canonical_under(target, *h))
        };
        let proof = match &got {
            Got::Value(proof) if !proof.is_null() => proof.clone(),
            Got::Transport => return self.failed("proofHash", &got, &request, epoch),
            _ => {
                if must_serve(epoch, inv, cmp) && !crate::chaos::fault_window_open() {
                    self.shared.violation(
                        "proofHash-missing",
                        epoch,
                        request,
                        json!("a proof of a canonical block"),
                        json!(got_json(&got)),
                    );
                }
                return;
            }
        };
        if self.shared.quirks.read().expect("quirks").contains(&target) {
            self.shared.count(&self.shared.reads, "quirkSkipped");
            return;
        }
        match proof_root(&proof) {
            Ok(root) if root == block.state_root => {}
            other => {
                self.shared.violation(
                    "proofHash-root",
                    epoch,
                    request,
                    json!(block.state_root),
                    json!({ "root": other.ok(), "proof": proof }),
                );
                return;
            }
        }
        if let Err(error) = verify_account_proof(address, &slots, block.state_root, &proof) {
            self.shared.violation(
                "proofHash-verify",
                epoch,
                request,
                json!(block.state_root),
                json!({ "error": error }),
            );
            return;
        }
        // Plain reads at the same block must agree with the verified proof.
        let checks: Vec<(&str, Value, Value)> = {
            let mut c = vec![
                ("eth_getBalance", json!([address, at]), proof["balance"].clone()),
                ("eth_getTransactionCount", json!([address, at]), proof["nonce"].clone()),
            ];
            if let Some(slot) = slots.first() {
                c.push((
                    "eth_getStorageAt",
                    json!([address, slot, at]),
                    proof["storageProof"][0]["value"].clone(),
                ));
            }
            c
        };
        for (method, params, expected) in checks {
            let (inv, cmp, epoch, got) = self.call(method, &params);
            self.shared.count(&self.shared.reads, method);
            let Got::Value(value) = &got else {
                if matches!(got, Got::Error(_))
                    && must_serve(epoch, inv, cmp)
                    && !crate::chaos::fault_window_open()
                {
                    self.shared.violation(
                        &format!("{method}-error"),
                        epoch,
                        json!([method, params]),
                        expected,
                        json!(got_json(&got)),
                    );
                }
                continue;
            };
            if big(value) != big(&expected) {
                self.shared.violation(
                    &format!("{method}-vs-proof"),
                    epoch,
                    json!([method, params]),
                    expected,
                    value.clone(),
                );
            }
        }
    }

    fn read_receipts(&mut self) {
        let rec = Arc::clone(&self.shared.recording);
        let Some(target) = self.sent_block() else { return };
        let number = rec.blocks[&target].number;
        let Some(oracle) =
            rec.oracle.get(&number).filter(|o| recording::hash(&o.block["hash"]) == Some(target))
        else {
            return;
        };
        let request = json!([{ "blockHash": target }]);
        let (_, _, epoch, got) = self.call("eth_getBlockReceipts", &request);
        self.shared.count(&self.shared.reads, "receipts");
        match &got {
            Got::Value(Value::Null) => {}
            Got::Value(receipts) => {
                if *receipts != oracle.receipts {
                    self.shared.violation(
                        "receipts",
                        epoch,
                        request,
                        oracle.receipts.clone(),
                        receipts.clone(),
                    );
                }
            }
            _ => self.failed("receipts", &got, &request, epoch),
        }
    }

    /// `eth_getLogs` by block hash: base-reth's receipts' logs for a block of its final chain.
    fn read_logs(&mut self) {
        let rec = Arc::clone(&self.shared.recording);
        let Some(target) = self.sent_block() else { return };
        let number = rec.blocks[&target].number;
        let Some(oracle) =
            rec.oracle.get(&number).filter(|o| recording::hash(&o.block["hash"]) == Some(target))
        else {
            return;
        };
        let expected: Vec<Value> = oracle
            .receipts
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|r| r["logs"].as_array().cloned().unwrap_or_default())
            .collect();
        let request = json!([{ "blockHash": target }]);
        let (_, _, epoch, got) = self.call("eth_getLogs", &request);
        self.shared.count(&self.shared.reads, "logs");
        match &got {
            Got::Value(logs) if *logs == json!(expected) => {}
            Got::Value(logs) => {
                self.shared.violation("logs", epoch, request, json!(expected), logs.clone())
            }
            // An unknown block (lost in a crash) is an error.
            Got::Error(_) => self.shared.count(&self.shared.failed, "logs:error"),
            Got::Transport => {}
        }
    }

    fn read_tx(&mut self) {
        let rec = Arc::clone(&self.shared.recording);
        let Some(target) = self.sent_block() else { return };
        let block = &rec.blocks[&target];
        if block.txs.is_empty() {
            return;
        }
        let tx = block.txs[self.rng.index(block.txs.len())];
        let request = json!([tx]);
        let (inv, cmp, epoch, got) = self.call("eth_getTransactionByHash", &request);
        self.shared.count(&self.shared.reads, "tx");
        let Got::Value(found) = &got else { return self.failed("tx", &got, &request, epoch) };
        let allowed = self.allowed(epoch, inv, cmp);
        let containing: Vec<B256> =
            rec.blocks.values().filter(|b| b.txs.contains(&tx)).map(|b| b.hash).collect();
        // The generator can repeat a deposit (same source hash) further up one chain. base-reth's
        // hash index then points at the later copy, and unwinding that block deletes the entry
        // while the earlier copy stays canonical, so base-reth answers null; tiger matches it.
        let repeated = containing
            .iter()
            .any(|a| containing.iter().any(|b| a != b && rec.is_canonical_under(*a, *b)));
        if repeated {
            self.shared.count(&self.shared.reads, "txRepeatedSkipped");
            return;
        }
        let canonical_somewhere =
            |h: &B256| containing.iter().any(|b| rec.is_canonical_under(*b, *h));
        if found.is_null() {
            if allowed.heads.iter().all(canonical_somewhere) {
                self.shared.violation(
                    "tx-missing",
                    epoch,
                    request,
                    json!({ "in": containing, "heads": allowed.heads }),
                    Value::Null,
                );
            }
            return;
        }
        let Some(in_block) = recording::hash(&found["blockHash"]) else { return };
        let ok = containing.contains(&in_block)
            && allowed.heads.iter().any(|h| rec.is_canonical_under(in_block, *h));
        if !ok {
            self.shared.violation(
                "tx-block",
                epoch,
                request,
                json!({ "in": containing, "heads": allowed.heads }),
                json!({ "blockHash": in_block, "blockNumber": found["blockNumber"], "window": [inv, cmp] }),
            );
        }
    }
}

fn got_json(got: &Got) -> Value {
    match got {
        Got::Value(v) => v.clone(),
        Got::Error(e) => json!({ "error": e }),
        Got::Transport => json!("transport"),
    }
}

/// A hex quantity or word as a number (`None` if malformed).
fn big(value: &Value) -> Option<U256> {
    let digits = value.as_str()?.strip_prefix("0x")?;
    if digits.is_empty() || digits.len() > 64 {
        return None;
    }
    U256::from_str_radix(digits, 16).ok()
}

/// The state root an `eth_getProof` answer proves against: the hash of its first account node.
pub fn proof_root(proof: &Value) -> Result<B256, String> {
    let nodes: Vec<Bytes> =
        serde_json::from_value(proof["accountProof"].clone()).map_err(|e| e.to_string())?;
    nodes.first().map(keccak256).ok_or_else(|| "empty account proof".to_owned())
}

/// Verifies an `eth_getProof` answer against `root`, taking its account fields as claimed.
pub fn verify_account_proof(
    address: Address,
    slots: &[B256],
    root: B256,
    proof: &Value,
) -> Result<(), String> {
    let nodes: Vec<Bytes> =
        serde_json::from_value(proof["accountProof"].clone()).map_err(|e| e.to_string())?;
    let balance = big(&proof["balance"]).ok_or("balance")?;
    let nonce = big(&proof["nonce"]).ok_or("nonce")?;
    let code_hash = recording::hash(&proof["codeHash"]).ok_or("codeHash")?;
    let storage_hash = recording::hash(&proof["storageHash"]).ok_or("storageHash")?;
    let absent = balance.is_zero()
        && nonce.is_zero()
        && (code_hash == B256::ZERO || code_hash == alloy_primitives::KECCAK256_EMPTY);
    let storage_root = if storage_hash == B256::ZERO { EMPTY_ROOT_HASH } else { storage_hash };
    let account = TrieAccount::new(
        nonce.to::<u64>(),
        balance,
        storage_root,
        if code_hash == B256::ZERO { alloy_primitives::KECCAK256_EMPTY } else { code_hash },
    );
    let key = Nibbles::unpack(keccak256(address));
    if let Err(error) = verify_proof(root, key, Some(alloy_rlp::encode(account)), &nodes) {
        if !(absent && storage_root == EMPTY_ROOT_HASH) {
            return Err(format!("account proof: {error}"));
        }
        verify_proof(root, key, None, &nodes).map_err(|e| format!("absence proof: {e}"))?;
    }
    let proofs = proof["storageProof"].as_array().cloned().unwrap_or_default();
    if proofs.len() != slots.len() {
        return Err(format!("{} storage proofs for {} slots", proofs.len(), slots.len()));
    }
    for (slot, sp) in slots.iter().zip(&proofs) {
        let value = big(&sp["value"]).ok_or("storage value")?;
        let nodes: Vec<Bytes> =
            serde_json::from_value(sp["proof"].clone()).map_err(|e| e.to_string())?;
        let expected = (!value.is_zero()).then(|| alloy_rlp::encode(value));
        verify_proof(storage_root, Nibbles::unpack(keccak256(slot)), expected, &nodes)
            .map_err(|e| format!("storage proof of {slot}: {e}"))?;
    }
    Ok(())
}
