//! Execution-layer sync check: a target node that only hears forkchoice updates must download
//! the chain from a source node over devp2p (the eth protocol), execute it, and end on the same
//! headers and state as the source, also when the source reorganizes during and after the sync.

use std::{
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use eyre::{WrapErr, eyre};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    Rng, StressPlan,
    nodes::{NodeClient, NodeProcess, Reply, StateOp, StateSession},
};

/// Settings of one sync check.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Canonical blocks the source has before the target starts.
    pub blocks: u64,
    /// Longest wait after the first forkchoice update for the target to download a block
    /// before the source reorganizes (the mid-sync reorg).
    pub reorg_after: Duration,
    /// How long the target may take to reach one head.
    pub timeout: Duration,
    /// Start the target without state at this block or tag of the source (a stateless start
    /// with the source as its witness upstream) instead of from genesis.
    pub stateless_start: Option<String>,
    /// Hand datadirs over between the clients: the target starts on a copy of the source's
    /// datadir (and continues the chain), and at the end the source's client opens a copy of
    /// the target's datadir and must serve what the source served.
    pub handoff: bool,
}

/// What one phase of the check saw.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPhase {
    /// Phase name.
    pub name: String,
    /// Source head number and hash the target was sent.
    pub head: (u64, B256),
    /// Target height when the phase's reorg happened, if it had one.
    pub target_height_at_reorg: Option<u64>,
    /// Forkchoice statuses the target answered, in order, with repeats collapsed.
    pub statuses: Vec<String>,
    /// Milliseconds until the target answered `VALID` on the head.
    pub millis: u128,
    /// Heights compared with the source.
    pub heights: u64,
    /// Differences from the source; empty when the phase passed.
    pub mismatches: Vec<String>,
}

/// The whole check's result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    /// Chain seed.
    pub seed: u64,
    /// Phases in order.
    pub phases: Vec<SyncPhase>,
    /// Divergences of the source from the generator while building (the source's own).
    pub source_divergences: Vec<String>,
    /// Stateless start asked for, if any.
    pub stateless_start: Option<String>,
    /// The source's finalized block when the target started (where a `finalized` start
    /// begins).
    pub source_finalized_at_start: Option<u64>,
    /// The lowest block the target serves; heights below it are absent (stateless start).
    pub first_block: Option<u64>,
    /// Account reads a stateless target answered with an error where the source answered but
    /// could not prove the account (`eth_getProof` failed), so the target had nowhere to fetch
    /// it from. Allowed (a clear error, never a wrong value), and counted. A refusal the source
    /// could have proven is a mismatch.
    pub partial_state_errors: Vec<String>,
}

/// A source node driven by a state session and a target node that syncs from it.
#[derive(Debug)]
pub struct SyncCheck {
    /// Session that builds the chain into the source node.
    pub session: StateSession,
    /// Target node client.
    pub target: NodeClient,
    /// Target process; its launch gets the source's enode in [`NodeProcess::PEERS_ENV`].
    pub proc: NodeProcess,
    /// Check settings.
    pub config: SyncConfig,
    rng: Rng,
    report: SyncReport,
}

impl SyncCheck {
    /// JSON-RPC error code tiger answers for a state read a stateless node cannot serve (state
    /// not in its partial datadir, upstream failed, or the upstream's proof did not verify).
    pub const STATE_NOT_LOCAL: i64 = -32020;

    /// A check of `target` syncing from the session's only node.
    pub fn new(
        session: StateSession,
        target: NodeClient,
        proc: NodeProcess,
        config: SyncConfig,
    ) -> Self {
        let seed = session.seed;
        Self {
            session,
            target,
            proc,
            config,
            rng: Rng::new(&[seed, 0x5e7c]),
            report: SyncReport {
                seed,
                phases: Vec::new(),
                source_divergences: Vec::new(),
                stateless_start: None,
                source_finalized_at_start: None,
                first_block: None,
                partial_state_errors: Vec::new(),
            },
        }
    }

    /// Runs every phase and returns the report: a full sync of `blocks` blocks with a reorg
    /// while it runs, a gap above the forward-download threshold, a short reorg, a reorg to a
    /// lower height, and a restart of the target followed by more blocks.
    pub fn run(mut self) -> eyre::Result<SyncReport> {
        self.extend(self.config.blocks)?;
        let enode = self.source_enode()?;
        self.proc.peers = Some(enode);
        if let Some(start) = self.config.stateless_start.clone() {
            let source = self.session.nodes.first().ok_or_else(|| eyre!("no source node"))?;
            let finalized = source.result("eth_getBlockByNumber", &json!(["finalized", false]))?;
            self.report.source_finalized_at_start = finalized["number"]
                .as_str()
                .and_then(|n| u64::from_str_radix(n.trim_start_matches("0x"), 16).ok());
            self.proc.stateless = Some(format!("{} {start}", source.http_url));
            self.report.stateless_start = Some(start);
        }
        if self.config.handoff {
            eyre::ensure!(self.config.stateless_start.is_none(), "handoff needs a full target");
            let source = &mut self.session.procs[0];
            source.stop(false);
            Self::copy_datadir(&source.datadir(), &self.proc.datadir())?;
            source.spawn(false)?;
            source.wait_ready(&self.session.nodes[0])?;
            self.proc.spawn(false)?;
        } else {
            self.proc.spawn(true)?;
        }
        self.proc.wait_ready(&self.target)?;

        let started = Instant::now();
        let mut statuses = Vec::new();
        self.send_forkchoice(&mut statuses);
        // Reorganize once the target has downloaded part of the chain, or after `reorg_after`.
        let mut at_reorg = self.target_height();
        while started.elapsed() < self.config.reorg_after && at_reorg.is_none_or(|h| h == 0) {
            thread::sleep(Duration::from_millis(50));
            at_reorg = self.target_height();
        }
        let depth = self.rng.range(4, 24) as u8;
        let blocks = self.rng.range(depth.into(), u64::from(StateOp::MAX_BLOCKS).max(depth.into()));
        self.fork(depth, blocks as u8)?;
        self.phase("full-sync-mid-reorg", started, statuses, at_reorg)?;

        self.extend(40)?;
        self.phase("gap-40", Instant::now(), Vec::new(), None)?;

        self.extend(3)?;
        self.fork(5, 8)?;
        self.phase("reorg-5", Instant::now(), Vec::new(), None)?;

        self.extend(40)?;
        self.fork(30, 3)?;
        self.phase("reorg-30-lower", Instant::now(), Vec::new(), None)?;

        self.proc.stop(true);
        self.extend(50)?;
        self.proc.spawn(false)?;
        self.proc.wait_ready(&self.target)?;
        self.phase("killed-restart-gap-50", Instant::now(), Vec::new(), None)?;
        self.proc.stop(false);
        if self.config.handoff {
            self.reopen_target_datadir()?;
        }
        Ok(self.report)
    }

    /// Copies a datadir without its p2p key, so the copy gets its own node id.
    fn copy_datadir(from: &std::path::Path, to: &std::path::Path) -> eyre::Result<()> {
        if to.exists() {
            std::fs::remove_dir_all(to)?;
        }
        let status = std::process::Command::new("cp").arg("-a").arg(from).arg(to).status()?;
        eyre::ensure!(status.success(), "copying {} to {}", from.display(), to.display());
        let _ = std::fs::remove_file(to.join("discovery-secret"));
        Ok(())
    }

    /// The final handoff: reads what the source serves (every block, the receipts and
    /// transactions of the last blocks, and account reads at several heights), stops it, starts
    /// the source's client on a copy of the target's datadir in `<root>/reopen`, and compares.
    /// Warnings in the reopened node's log about healing, rebuilding, or unwinding are reported
    /// too.
    fn reopen_target_datadir(&mut self) -> eyre::Result<()> {
        let started = Instant::now();
        let head = self.session.tree.head();
        let head = (head.number, head.hash);
        let source = self.session.nodes[0].clone();
        let mut requests: Vec<(&str, Value)> = Vec::new();
        for number in 0..=head.0 {
            requests.push(("eth_getBlockByNumber", json!([format!("{number:#x}"), false])));
        }
        let mut addresses = Vec::new();
        for number in head.0.saturating_sub(10)..=head.0 {
            let tag = format!("{number:#x}");
            requests.push(("eth_getBlockReceipts", json!([tag])));
            let block = source.result("eth_getBlockByNumber", &json!([tag, true]))?;
            for tx in block["transactions"].as_array().into_iter().flatten() {
                requests.push(("eth_getTransactionByHash", json!([tx["hash"]])));
                addresses.extend([tx["from"].clone(), tx["to"].clone()]);
            }
        }
        addresses.retain(|a| !a.is_null());
        addresses.sort_by_key(Value::to_string);
        addresses.dedup();
        for number in [head.0, head.0.saturating_sub(20), head.0.saturating_sub(60)] {
            for address in addresses.iter().take(30) {
                for method in ["eth_getBalance", "eth_getTransactionCount", "eth_getCode"] {
                    requests.push((method, json!([address, format!("{number:#x}")])));
                }
            }
        }
        let expected: Vec<Reply> = requests.iter().map(|(m, p)| source.call(m, p)).collect();
        let source_proc = &mut self.session.procs[0];
        source_proc.stop(false);
        let root = source_proc.root.join("reopen");
        std::fs::create_dir_all(&root)?;
        for file in ["genesis.json", "jwt.hex"] {
            std::fs::copy(source_proc.root.join(file), root.join(file))?;
        }
        let mut reopened =
            NodeProcess::new(source_proc.name.clone(), source_proc.launch.clone(), root);
        Self::copy_datadir(&self.proc.datadir(), &reopened.datadir())?;
        reopened.spawn(false)?;
        let mut mismatches = Vec::new();
        match reopened.wait_ready(&source) {
            Err(error) => mismatches.push(format!("source client did not start: {error:#}")),
            Ok(_) => {
                for ((method, params), want) in requests.iter().zip(&expected) {
                    let got = source.call(method, params);
                    if &got != want && mismatches.len() < 20 {
                        mismatches
                            .push(format!("{method} {params}: source {want:?} reopened {got:?}"));
                    }
                }
            }
        }
        reopened.stop(false);
        let log = std::fs::read_to_string(reopened.log_path()).unwrap_or_default();
        // base-reth logs "Healing static file inconsistencies." at INFO on every start; a real
        // repair or rebuild warns.
        for line in log.lines().filter(|l| {
            let l = l.to_lowercase();
            (l.contains("warn") || l.contains("error"))
                && ["heal", "rebuild", "inconsisten", "unwind"].iter().any(|w| l.contains(w))
        }) {
            if mismatches.len() < 40 {
                mismatches.push(format!("reopened log: {line}"));
            }
        }
        eprintln!("sync: source-client-opens-target-datadir {} mismatches", mismatches.len());
        self.report.phases.push(SyncPhase {
            name: "source-client-opens-target-datadir".to_owned(),
            head,
            target_height_at_reorg: None,
            statuses: Vec::new(),
            millis: started.elapsed().as_millis(),
            heights: head.0 + 1,
            mismatches,
        });
        Ok(())
    }

    fn extend(&mut self, mut blocks: u64) -> eyre::Result<()> {
        while blocks > 0 {
            let step = blocks.min(u64::from(StateOp::MAX_BLOCKS));
            let op = StateOp::Extend {
                blocks: step as u8,
                stress: StressPlan::random(&mut self.rng),
                salt: self.rng.next_u64(),
                fcu_each: false,
            };
            self.source_op(&op)?;
            blocks -= step;
        }
        Ok(())
    }

    fn fork(&mut self, depth: u8, blocks: u8) -> eyre::Result<()> {
        let op = StateOp::Fork {
            depth,
            blocks,
            stress: StressPlan::random(&mut self.rng),
            salt: self.rng.next_u64(),
            switch: true,
        };
        self.source_op(&op)
    }

    fn source_op(&mut self, op: &StateOp) -> eyre::Result<()> {
        for divergence in self.session.run(op)? {
            self.report.source_divergences.push(divergence.signature());
        }
        Ok(())
    }

    /// The source's enode from `admin_nodeInfo`, with its host replaced by loopback.
    fn source_enode(&self) -> eyre::Result<String> {
        let source = self.session.nodes.first().ok_or_else(|| eyre!("no source node"))?;
        let info = source.result("admin_nodeInfo", &json!([]))?;
        let enode = info["enode"].as_str().ok_or_else(|| eyre!("admin_nodeInfo has no enode"))?;
        let (id, rest) = enode.split_once('@').ok_or_else(|| eyre!("odd enode {enode}"))?;
        let port = rest.split(['?', ':']).nth(1).ok_or_else(|| eyre!("odd enode {enode}"))?;
        Ok(format!("{id}@127.0.0.1:{port}"))
    }

    fn target_height(&self) -> Option<u64> {
        let number = self.target.result("eth_blockNumber", &json!([])).ok()?;
        u64::from_str_radix(number.as_str()?.trim_start_matches("0x"), 16).ok()
    }

    /// Sends the target the session's current forkchoice state and records the status.
    fn send_forkchoice(&mut self, statuses: &mut Vec<String>) -> String {
        let params = self.session.forkchoice_state(self.session.tree.head);
        let status = match self.target.engine("engine_forkchoiceUpdatedV3", &params) {
            Ok(result) => result["payloadStatus"]["status"].as_str().unwrap_or("?").to_owned(),
            Err(error) => format!("error: {error:#}"),
        };
        if statuses.last() != Some(&status) {
            statuses.push(status.clone());
        }
        status
    }

    /// Moves the target to the source head, waits for `VALID`, and compares every height.
    fn phase(
        &mut self,
        name: &str,
        started: Instant,
        mut statuses: Vec<String>,
        target_height_at_reorg: Option<u64>,
    ) -> eyre::Result<()> {
        let head = self.session.tree.head();
        let head = (head.number, head.hash);
        let mut mismatches = Vec::new();
        loop {
            let status = self.send_forkchoice(&mut statuses);
            if status == "VALID" {
                break;
            }
            if status != "SYNCING" && status != "ACCEPTED" {
                mismatches.push(format!("forkchoice answered {status}"));
                break;
            }
            if started.elapsed() > self.config.timeout {
                mismatches.push(format!(
                    "not VALID after {:?}; target at {:?}",
                    self.config.timeout,
                    self.target_height()
                ));
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        let millis = started.elapsed().as_millis();
        let heights = if mismatches.is_empty() { self.compare(head, &mut mismatches)? } else { 0 };
        eprintln!(
            "sync: {name} head {} {} in {millis} ms, {} mismatches",
            head.0,
            head.1,
            mismatches.len()
        );
        self.report.phases.push(SyncPhase {
            name: name.to_owned(),
            head,
            target_height_at_reorg,
            statuses,
            millis,
            heights,
            mismatches,
        });
        Ok(())
    }

    /// Compares the target with the source at every height it serves up to the head: block
    /// hash, state root, receipts root, and the accounts of the head block's transactions. A
    /// full target serves every height; a stateless one the heights from its start block, which
    /// must not move between phases.
    fn compare(&mut self, head: (u64, B256), mismatches: &mut Vec<String>) -> eyre::Result<u64> {
        let source = self.session.nodes.first().ok_or_else(|| eyre!("no source node"))?;
        let latest = self.target.result("eth_getBlockByNumber", &json!(["latest", false]))?;
        if latest["hash"] != json!(head.1) {
            mismatches.push(format!("target latest {} is not head {}", latest["hash"], head.1));
        }
        let mut first = 0;
        if self.config.stateless_start.is_some() {
            while first <= head.0
                && self
                    .target
                    .result("eth_getBlockByNumber", &json!([format!("{first:#x}"), false]))?
                    .is_null()
            {
                first += 1;
            }
            match self.report.first_block {
                None => self.report.first_block = Some(first),
                Some(was) if was != first => {
                    mismatches.push(format!("first served block moved from {was} to {first}"));
                }
                Some(_) => {}
            }
        }
        for number in first..=head.0 {
            let params = json!([format!("{number:#x}"), false]);
            let ours = source.result("eth_getBlockByNumber", &params)?;
            let theirs = self.target.result("eth_getBlockByNumber", &params)?;
            for field in ["hash", "stateRoot", "receiptsRoot", "transactionsRoot", "logsBloom"] {
                if ours[field] != theirs[field] {
                    mismatches.push(format!(
                        "block {number} {field}: source {} target {}",
                        ours[field], theirs[field]
                    ));
                }
            }
            if mismatches.len() > 20 {
                break;
            }
        }
        let block = source
            .result("eth_getBlockByNumber", &json!([format!("{:#x}", head.0), true]))
            .wrap_err("reading the head block")?;
        let mut addresses: Vec<Value> = vec![block["miner"].clone()];
        for tx in block["transactions"].as_array().into_iter().flatten() {
            addresses.extend([tx["from"].clone(), tx["to"].clone()]);
        }
        addresses.retain(|a| !a.is_null());
        addresses.dedup();
        for address in addresses.iter().take(24) {
            for method in ["eth_getBalance", "eth_getTransactionCount", "eth_getCode"] {
                let params = json!([address, format!("{:#x}", head.0)]);
                let ours = source.call(method, &params);
                let theirs = self.target.call(method, &params);
                // Tiger's documented refusal of state a stateless node does not hold.
                let refused = matches!(&theirs, Reply::Body(body)
                    if body["error"]["code"] == json!(Self::STATE_NOT_LOCAL))
                    && matches!(&ours, Reply::Body(body) if body.get("result").is_some());
                // A stateless target fills reads it does not hold from the upstream's proofs (the
                // source here), so a refusal is allowed only when the source cannot prove it.
                let unprovable = refused
                    && self.config.stateless_start.is_some()
                    && source
                        .result("eth_getProof", &json!([address, [], format!("{:#x}", head.0)]))
                        .is_err();
                if unprovable {
                    self.report
                        .partial_state_errors
                        .push(format!("{method} {address} at {}: {theirs:?}", head.0));
                } else if ours != theirs {
                    mismatches.push(format!(
                        "{method} {address} at {}: source {ours:?} target {theirs:?}",
                        head.0
                    ));
                }
            }
        }
        Ok(head.0 + 1 - first)
    }
}
