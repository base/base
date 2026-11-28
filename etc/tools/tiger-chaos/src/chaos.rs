//! The chaos driver: replays a recording into tiger while killing it, arming store faults,
//! flapping the forkchoice, and letting reader threads check every read against the history.
//! After every recovery it checks the reopened chain and state against the recording, and at
//! the end of a run it compares tiger's chain with base-reth's answers and opens tiger's datadir
//! with base-reth.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use base_diff_fuzz::{NodeClient, NodeProcess, Rng};
use eyre::{WrapErr, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    Fcu, Reader, Recording, Shared, StepKind, Timeline, Violation,
    history::{Epoch, proof_root, verify_account_proof},
    recording::{self, copy_dir},
};

/// Set while a run has an `enospc` or `eio` fault armed: reads and writes may fail with the
/// disk error, which is not a violation.
static FAULT_WINDOW: AtomicBool = AtomicBool::new(false);

/// Whether a disk-error fault window is open.
pub fn fault_window_open() -> bool {
    FAULT_WINDOW.load(Ordering::SeqCst)
}

/// Settings of a chaos campaign.
#[derive(Debug, Clone)]
pub struct ChaosConfig {
    /// Recording directory.
    pub recording: PathBuf,
    /// Directory of the runs' datadirs and logs.
    pub root: PathBuf,
    /// Launch script (`etc/tools/tiger-chaos/launch.sh`).
    pub launch: PathBuf,
    /// The tiger binary.
    pub tiger_bin: PathBuf,
    /// The base-reth binary (`base-reth-node` or `base-diff-node`).
    pub base_bin: PathBuf,
    /// Port offset of every node.
    pub port_offset: u16,
    /// Reader threads.
    pub readers: usize,
    /// Campaign seed.
    pub seed: u64,
    /// Chaos actions per 1000 steps (each kind scales with it).
    pub intensity: u64,
    /// Fault modes the tiger build supports (`torn`, `lose_unsynced`, `enospc`, `eio`, `fsync_eio`).
    pub fault_modes: Vec<String>,
    /// Percentage of runs that start from base-reth's mid-recording snapshot.
    pub snapshot_percent: u64,
    /// Percentage of kills after which base-reth opens a copy of the crashed datadir first.
    pub crash_handoff_percent: u64,
}

/// What one run did.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    /// Run index.
    pub run: u64,
    /// Run seed.
    pub seed: u64,
    /// `genesis` or `snapshot`.
    pub start: String,
    /// tiger's persistence flags.
    pub persistence: String,
    /// Steps replayed.
    pub steps: usize,
    /// Chaos actions and recoveries by kind.
    pub events: BTreeMap<String, u64>,
    /// Reads by kind.
    pub reads: BTreeMap<String, u64>,
    /// Failed reads by kind.
    pub failed_reads: BTreeMap<String, u64>,
    /// Violations.
    pub violations: Vec<Violation>,
    /// Seconds taken.
    pub seconds: f64,
}

/// One chaos run.
#[derive(Debug)]
pub struct ChaosRun {
    config: ChaosConfig,
    shared: Arc<Shared>,
    dir: PathBuf,
    tiger: NodeClient,
    proc: NodeProcess,
    rng: Rng,
    /// tiger's head as the driver believes it.
    head: B256,
    /// The recording's latest safe and finalized blocks.
    safe: B256,
    finalized: B256,
    /// Every head tiger was moved to or reopened at.
    heads_seen: HashSet<B256>,
    /// Set before the driver kills tiger on purpose.
    expected_death: Arc<AtomicBool>,
    /// Kill thread in flight.
    killer: Option<thread::JoinHandle<()>>,
    events: BTreeMap<String, u64>,
    /// Log of driver actions, for reproducers.
    actions: Vec<Value>,
}

impl ChaosRun {
    /// tiger's HTTP and Engine API ports.
    fn tiger_spec(offset: u16) -> String {
        format!("tiger=http://127.0.0.1:{},http://127.0.0.1:{}", 28545 + offset, 28551 + offset)
    }

    /// base-reth's ports when it opens a datadir tiger left (launch name `handoff`).
    fn handoff_spec(offset: u16) -> String {
        format!("handoff=http://127.0.0.1:{},http://127.0.0.1:{}", 19545 + offset, 19551 + offset)
    }

    /// Runs run `run` of the campaign and returns its report; its directory is
    /// `<root>/run-<run>` and keeps the logs and `report.json`.
    pub fn execute(
        config: &ChaosConfig,
        recording: &Arc<Recording>,
        run: u64,
    ) -> eyre::Result<RunReport> {
        let seed = config.seed.wrapping_mul(1_000_003).wrapping_add(run);
        let mut rng = Rng::new(&[seed, 0xc4a05]);
        let dir = config.root.join(format!("run-{run}"));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        fs::copy(config.recording.join("genesis.json"), dir.join("genesis.json"))?;
        let jwt = dir.join("jwt.hex");
        fs::write(&jwt, alloy_primitives::hex::encode(B256::random()))?;
        let (threshold, target) =
            *rng.pick(&[(1u64, 0u64), (2, 1), (2, 1), (8, 2), (16, 4), (70, 5)]);
        let persistence = format!(
            "--engine.persistence-threshold {threshold} --engine.memory-block-buffer-target {target}"
        );
        fs::write(
            dir.join("launch.env"),
            format!(
                "PORT_OFFSET={}\nTIGER_BIN={}\nBASE_NODE_BIN={}\nTIGER_EXTRA=\"{persistence}\"\n",
                config.port_offset,
                config.tiger_bin.display(),
                config.base_bin.display()
            ),
        )?;
        let tiger = NodeClient::parse(&Self::tiger_spec(config.port_offset), &jwt)?;
        let proc = NodeProcess::new("tiger".into(), config.launch.clone(), dir.clone());
        let snapshot = recording.snapshot.clone().filter(|_| rng.chance(config.snapshot_percent));
        let shared = Arc::new(Shared {
            start: Instant::now(),
            recording: Arc::clone(recording),
            timeline: Mutex::new(Timeline::default()),
            open: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            progress: AtomicUsize::new(snapshot.as_ref().map_or(0, |s| s.steps)),
            stop: AtomicBool::new(false),
            violations: Mutex::new(Vec::new()),
            quirks: if snapshot.is_some() {
                Self::base_reth_quirks(config, recording)?
            } else {
                HashSet::new()
            },
            reads: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(BTreeMap::new()),
        });
        let mut this = Self {
            config: config.clone(),
            shared,
            dir: dir.clone(),
            tiger,
            proc,
            rng,
            head: recording.genesis,
            safe: recording.genesis,
            finalized: recording.genesis,
            heads_seen: HashSet::from([recording.genesis]),
            expected_death: Arc::new(AtomicBool::new(false)),
            killer: None,
            events: BTreeMap::new(),
            actions: Vec::new(),
        };
        let mut report = RunReport {
            run,
            seed,
            start: if snapshot.is_some() { "snapshot".into() } else { "genesis".into() },
            persistence,
            ..RunReport::default()
        };
        let mut first = 0;
        if let Some(snapshot) = &snapshot {
            copy_dir(&recording::snapshot_datadir(&config.recording), &dir.join("tiger"))?;
            // A base-reth datadir carries its p2p key; tiger keeps its own.
            let _ = fs::remove_file(dir.join("tiger/discovery-secret"));
            first = snapshot.steps;
            this.head = snapshot.head;
            this.safe = snapshot.safe;
            this.finalized = snapshot.finalized;
            this.heads_seen.insert(snapshot.head);
        }
        FAULT_WINDOW.store(false, Ordering::SeqCst);
        if snapshot.is_none() {
            let _ = fs::remove_dir_all(this.proc.datadir());
        }
        fs::create_dir_all(this.proc.datadir())?;
        append_log(&this.proc.log_path(), "launch: start\n");
        this.proc.spawn(false)?;
        this.proc.wait_ready(&this.tiger)?;
        let start_head = this.head;
        this.open_epoch(Some(start_head))?;
        let readers = Reader::spawn(&this.shared, &this.tiger, config.readers, seed);
        let result = this.drive(first);
        this.shared.stop.store(true, Ordering::SeqCst);
        for reader in readers {
            let _ = reader.join();
        }
        if let Some(killer) = this.killer.take() {
            let _ = killer.join();
        }
        if let Err(error) = &result {
            this.shared.violation(
                "harness",
                this.epoch(),
                json!("run"),
                json!("completion"),
                json!(format!("{error:#}")),
            );
        }
        this.proc.stop(true);
        report.steps = this.shared.progress.load(Ordering::SeqCst);
        report.events = this.events.clone();
        report.reads = this.shared.reads.lock().expect("reads").clone();
        report.failed_reads = this.shared.failed.lock().expect("failed").clone();
        report.violations = this.shared.violations.lock().expect("violations").clone();
        report.seconds = this.shared.now();
        fs::write(dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        fs::write(dir.join("actions.json"), serde_json::to_vec_pretty(&this.actions)?)?;
        let timeline = this.shared.timeline.lock().expect("timeline");
        fs::write(
            dir.join("timeline.json"),
            serde_json::to_vec(&json!({ "epochs": timeline.epochs, "fcus": timeline.fcus }))?,
        )?;
        Ok(report)
    }

    /// The snapshot's canonical blocks at which base-reth, on its own snapshot datadir, answers
    /// `eth_getProof` against a root other than the block's state root. tiger on that datadir
    /// answers the same, so runs started there skip those blocks' roots. Probed once per
    /// recording and cached in `quirks.json`.
    pub fn base_reth_quirks(config: &ChaosConfig, rec: &Recording) -> eyre::Result<HashSet<B256>> {
        let cache = config.recording.join("quirks.json");
        if let Ok(text) = fs::read_to_string(&cache) {
            return Ok(serde_json::from_str(&text)?);
        }
        let Some(snapshot) = &rec.snapshot else { return Ok(HashSet::new()) };
        let dir = config.root.join("quirks");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        fs::copy(config.recording.join("genesis.json"), dir.join("genesis.json"))?;
        fs::write(dir.join("jwt.hex"), alloy_primitives::hex::encode(B256::random()))?;
        fs::write(
            dir.join("launch.env"),
            format!(
                "PORT_OFFSET={}\nBASE_NODE_BIN={}\n",
                config.port_offset,
                config.base_bin.display()
            ),
        )?;
        copy_dir(&recording::snapshot_datadir(&config.recording), &dir.join("handoff"))?;
        let node =
            NodeClient::parse(&Self::handoff_spec(config.port_offset), &dir.join("jwt.hex"))?;
        let mut proc = NodeProcess::new("handoff".into(), config.launch.clone(), dir.clone());
        proc.spawn(false)?;
        proc.wait_ready(&node)?;
        let mut quirks = HashSet::new();
        for number in 0..=rec.blocks[&snapshot.head].number {
            let Some(at) = rec.ancestor_at(snapshot.head, number) else { continue };
            let proof = node.result(
                "eth_getProof",
                &json!(["0x4200000000000000000000000000000000000015", [], format!("{number:#x}")]),
            )?;
            if proof_root(&proof).ok() != Some(rec.blocks[&at].state_root) {
                quirks.insert(at);
            }
        }
        proc.stop(false);
        let _ = fs::remove_dir_all(&dir);
        eprintln!("base-reth's own proofs miss the state root at {} snapshot blocks", quirks.len());
        fs::write(&cache, serde_json::to_vec(&quirks)?)?;
        Ok(quirks)
    }

    fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::SeqCst)
    }

    fn event(&mut self, kind: &str, detail: Value) {
        *self.events.entry(kind.to_owned()).or_default() += 1;
        let step = self.shared.progress.load(Ordering::SeqCst);
        self.actions.push(
            json!({ "at": self.shared.now(), "step": step, "event": kind, "detail": detail }),
        );
    }

    fn violation(&self, kind: &str, request: Value, expected: Value, got: Value) {
        self.shared.violation(kind, self.epoch(), request, expected, got);
    }

    fn rec(&self) -> Arc<Recording> {
        Arc::clone(&self.shared.recording)
    }

    /// Replays the steps from `first`, then runs the end-of-run checks.
    fn drive(&mut self, first: usize) -> eyre::Result<()> {
        let rec = self.rec();
        let per_mille = self.config.intensity;
        for index in first..rec.steps.len() {
            if self.rng.below(1000) < per_mille {
                self.chaos_action()?;
            }
            self.run_step(index)?;
            self.shared.progress.store(index + 1, Ordering::SeqCst);
            if self
                .shared
                .violations
                .lock()
                .expect("violations")
                .iter()
                .any(|v| v.kind == "reopen-failed")
            {
                return Ok(());
            }
        }
        self.finish()
    }

    /// Recovers first when tiger died since the last call (a kill thread fired, or an armed
    /// fault crashed it in the background).
    fn settle(&mut self) -> eyre::Result<()> {
        if let Some(killer) = self.killer.take() {
            let _ = killer.join();
        }
        if !self.proc.alive()
            || self.expected_death.load(Ordering::SeqCst)
            || self.proc.injected_crash().is_some()
        {
            self.handle_failure("node down between steps")?;
        }
        Ok(())
    }

    /// One random chaos action between steps.
    fn chaos_action(&mut self) -> eyre::Result<()> {
        self.settle()?;
        match self.rng.weighted(&[16, 22, 8, 10, 24, 20]) {
            0 => {
                self.event("kill", json!(null));
                self.kill_now();
                self.recover(false)?;
            }
            1 => {
                let delay = self.rng.below(400);
                self.event("killDuring", json!({ "delayMs": delay }));
                self.kill_after(Duration::from_millis(delay), false);
            }
            2 => {
                self.event("term", json!(null));
                self.close();
                self.proc.stop(false);
                let clean =
                    self.log_since_start().contains("persisted every block through the head");
                self.recover(clean)?;
            }
            3 => {
                let delay = self.rng.below(1500);
                self.event("termThenKill", json!({ "delayMs": delay }));
                self.close();
                self.kill_after(Duration::from_millis(delay), true);
                if let Some(killer) = self.killer.take() {
                    let _ = killer.join();
                }
                // Whether the clean stop finished before the kill decides what must survive.
                let clean =
                    self.log_since_start().contains("persisted every block through the head");
                self.proc.stop(true);
                self.recover(clean)?;
            }
            4 => self.arm_fault()?,
            _ => self.flap()?,
        }
        Ok(())
    }

    /// The current process's log.
    fn log_since_start(&self) -> String {
        let text = fs::read(self.proc.log_path()).unwrap_or_default();
        let text = String::from_utf8_lossy(&text).into_owned();
        // The last launch line starts the current process's part.
        match text.rfind("launch: start") {
            Some(at) => text[at..].to_owned(),
            None => text,
        }
    }

    fn close(&self) {
        self.shared.open.store(false, Ordering::SeqCst);
    }

    /// The node's processes, found by their datadir argument.
    fn node_pids(datadir: &Path) -> Vec<i32> {
        let needle = datadir.to_string_lossy().into_owned();
        let me = std::process::id() as i32;
        let mut pids = Vec::new();
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
            if pid == me {
                continue;
            }
            let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else { continue };
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            if args.contains(&needle.as_bytes()) && args.contains(&b"node".as_slice()) {
                pids.push(pid);
            }
        }
        pids
    }

    fn kill_now(&mut self) {
        self.close();
        self.expected_death.store(true, Ordering::SeqCst);
        self.proc.stop(true);
    }

    /// Kills tiger after `delay` from another thread (`term`: SIGTERM first, SIGKILL after the
    /// delay), so the kill lands inside whatever tiger is doing then.
    fn kill_after(&mut self, delay: Duration, term: bool) {
        if let Some(killer) = self.killer.take() {
            let _ = killer.join();
        }
        let datadir = self.proc.datadir();
        let expected = Arc::clone(&self.expected_death);
        if term {
            for pid in Self::node_pids(&datadir) {
                // SAFETY: kill has no memory-safety preconditions.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        self.killer = Some(thread::spawn(move || {
            thread::sleep(delay);
            expected.store(true, Ordering::SeqCst);
            for pid in Self::node_pids(&datadir) {
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }));
    }

    /// Restarts tiger cleanly with a store fault armed for the new process.
    fn arm_fault(&mut self) -> eyre::Result<()> {
        let mode = self.rng.pick(&self.config.fault_modes).clone();
        let limit = *self.rng.pick(&[40u64, 300, 2_000, 8_000]);
        let writes = self.rng.range(1, limit);
        let seed = self.rng.next_u64() % 1_000_000;
        let mut spec = format!("seed={seed},crash_after_writes={writes},mode={mode}");
        if mode == "enospc" || mode == "eio" {
            let count = *self.rng.pick(&[1u64, 1, 2, 6, 30]);
            spec.push_str(&format!(",fail_writes={count}"));
        }
        self.event("fault", json!(spec));
        self.close();
        self.proc.stop(false);
        let log = self.log_since_start();
        let clean = log.contains("persisted every block through the head");
        if !clean && !log.contains("tiger-fault:") {
            self.violation(
                "stop",
                json!("SIGTERM before arming a fault"),
                json!("clean stop"),
                json!(self.log_tail()),
            );
        }
        self.proc.fault = Some(spec);
        FAULT_WINDOW
            .store(mode == "enospc" || mode == "eio" || mode == "fsync_eio", Ordering::SeqCst);
        self.recover(clean)
    }

    /// Moves the head among sent branch tips a few times, then back to the recording's head.
    fn flap(&mut self) -> eyre::Result<()> {
        let rec = self.rec();
        let progress = self.shared.progress.load(Ordering::SeqCst);
        let finalized_number = rec.blocks[&self.finalized].number;
        let candidates: Vec<B256> = rec.steps[..progress]
            .iter()
            .filter_map(|s| match s.kind {
                StepKind::Payload { hash } => Some(hash),
                _ => None,
            })
            .filter(|h| {
                let b = &rec.blocks[h];
                b.number > finalized_number
                    && rec.is_canonical_under(self.finalized, *h)
                    && !rec.is_canonical_under(*h, self.head)
            })
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let back = self.head;
        let (safe, finalized) = (self.safe, self.finalized);
        let count = self.rng.range(2, 9);
        self.event("flap", json!({ "count": count }));
        for _ in 0..count {
            let target = *self.rng.pick(&candidates);
            self.forkchoice(target, self.finalized, self.finalized)?;
        }
        self.forkchoice(back, safe, finalized)
    }

    /// Sends step `index`, recovering from crashes and resending lost blocks until it is done.
    fn run_step(&mut self, index: usize) -> eyre::Result<()> {
        let rec = self.rec();
        let step = &rec.steps[index];
        match step.kind {
            StepKind::Payload { hash } => self.payload(hash),
            StepKind::Invalid { parent } => {
                for _ in 0..200 {
                    match self.tiger.engine(&step.method, &step.params) {
                        Ok(status) => match status["status"].as_str() {
                            Some("INVALID") => return Ok(()),
                            Some("SYNCING") => self.ensure_chain(parent)?,
                            _ => {
                                self.violation(
                                    "invalid-accepted",
                                    json!(index),
                                    json!("INVALID"),
                                    status,
                                );
                                return Ok(());
                            }
                        },
                        Err(error) => self.handle_failure(&error.to_string())?,
                    }
                }
                bail!("step {index} did not finish")
            }
            StepKind::Forkchoice { head, safe, finalized } => {
                self.safe = safe;
                self.finalized = finalized;
                self.forkchoice(head, safe, finalized)
            }
        }
    }

    /// Submits block `hash` until tiger takes it.
    fn payload(&mut self, hash: B256) -> eyre::Result<()> {
        let rec = self.rec();
        let block = &rec.blocks[&hash];
        let step = &rec.steps[block.step.expect("submitted block")];
        for _ in 0..200 {
            match self.tiger.engine(&step.method, &step.params) {
                Ok(status) => match status["status"].as_str() {
                    Some("VALID" | "ACCEPTED") => return Ok(()),
                    Some("SYNCING") => self.ensure_chain(block.parent)?,
                    _ => {
                        self.violation(
                            "payload",
                            json!({ "block": hash, "number": block.number }),
                            json!("VALID"),
                            status,
                        );
                        return Ok(());
                    }
                },
                Err(error) => self.handle_failure(&error.to_string())?,
            }
        }
        bail!("block {hash} was never taken")
    }

    /// Resends the blocks of the chain ending at `hash` that tiger does not have.
    fn ensure_chain(&mut self, hash: B256) -> eyre::Result<()> {
        let rec = self.rec();
        let mut missing = Vec::new();
        let mut at = hash;
        while at != rec.genesis {
            match self.tiger.result("eth_getBlockByHash", &json!([at, false])) {
                Ok(block) if !block.is_null() => break,
                Ok(_) => {}
                Err(error) => {
                    self.handle_failure(&error.to_string())?;
                    continue;
                }
            }
            missing.push(at);
            at = rec.blocks[&at].parent;
        }
        if !missing.is_empty() {
            self.event("resend", json!(missing.len()));
        }
        for block in missing.into_iter().rev() {
            self.payload(block)?;
        }
        Ok(())
    }

    /// Sends a forkchoice update until tiger acknowledges it, logging it in the timeline.
    fn forkchoice(&mut self, head: B256, safe: B256, finalized: B256) -> eyre::Result<()> {
        let rec = self.rec();
        for _ in 0..200 {
            // base-reth keeps its head on an update naming one of the head's ancestors.
            let effective = if head != self.head && rec.is_canonical_under(head, self.head) {
                self.head
            } else {
                head
            };
            let epoch = self.epoch();
            let index = {
                let mut timeline = self.shared.timeline.lock().expect("timeline");
                timeline.fcus.push(Fcu {
                    epoch,
                    sent: self.shared.now(),
                    acked: None,
                    head: effective,
                    safe,
                    finalized,
                });
                timeline.fcus.len() - 1
            };
            let state = json!({ "headBlockHash": head, "safeBlockHash": safe, "finalizedBlockHash": finalized });
            match self.tiger.engine("engine_forkchoiceUpdatedV3", &json!([state, null])) {
                Ok(result) => match result["payloadStatus"]["status"].as_str() {
                    Some("VALID") => {
                        self.shared.timeline.lock().expect("timeline").fcus[index].acked =
                            Some(self.shared.now());
                        self.head = effective;
                        self.heads_seen.insert(effective);
                        return Ok(());
                    }
                    Some("SYNCING") => self.ensure_chain(head)?,
                    _ => {
                        self.violation("forkchoice", state, json!("VALID"), result);
                        return Ok(());
                    }
                },
                Err(error) => self.handle_failure(&error.to_string())?,
            }
        }
        bail!("forkchoice to {head} never taken")
    }

    /// Handles a failed Engine API call: waits out a disk-error window, or recovers a dead node.
    fn handle_failure(&mut self, error: &str) -> eyre::Result<()> {
        if self.proc.alive()
            && self.proc.injected_crash().is_none()
            && !self.expected_death.load(Ordering::SeqCst)
        {
            if fault_window_open() {
                self.event("diskErrorRetry", json!(null));
                thread::sleep(Duration::from_millis(300));
                return Ok(());
            }
            // The node may be dying from a kill in flight; give it a moment.
            thread::sleep(Duration::from_millis(300));
            if self.proc.alive() && !self.expected_death.load(Ordering::SeqCst) {
                self.violation(
                    "engine-error",
                    json!("engine call"),
                    json!("an answer"),
                    json!(error),
                );
                thread::sleep(Duration::from_millis(700));
                return Ok(());
            }
        }
        if let Some(killer) = self.killer.take() {
            let _ = killer.join();
        }
        self.close();
        // Wait for the process to exit when it is going down by itself.
        let started = Instant::now();
        while self.proc.alive() && started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        self.proc.stop(true);
        let log = self.log_since_start();
        let expected = self.expected_death.load(Ordering::SeqCst);
        let fault = log.lines().find(|l| l.contains("tiger-fault:")).map(str::to_owned);
        match (&fault, expected) {
            (Some(line), _) => self.event("faultCrash", json!(line)),
            (None, true) => self.event("killed", json!(null)),
            (None, false) => {
                let tail = self.log_tail();
                self.violation(
                    "crash",
                    json!("node exited"),
                    json!("no exit without an injected fault"),
                    json!(tail),
                );
            }
        }
        self.recover(false)
    }

    fn log_tail(&self) -> Vec<String> {
        let log = self.log_since_start();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(30)..].iter().map(|l| (*l).to_owned()).collect()
    }

    /// Restarts tiger (unarmed unless a fault was just armed), checks the reopened chain, and
    /// opens a new epoch. `clean`: the last process stopped cleanly, so every block through its
    /// head must have survived.
    fn recover(&mut self, clean: bool) -> eyre::Result<()> {
        if let Some(killer) = self.killer.take() {
            let _ = killer.join();
        }
        self.proc.stop(true);
        self.expected_death.store(false, Ordering::SeqCst);
        if self.proc.fault.is_none() {
            FAULT_WINDOW.store(false, Ordering::SeqCst);
        }
        if !clean && self.rng.chance(self.config.crash_handoff_percent) {
            self.crash_handoff()?;
        }
        let armed = self.proc.fault.is_some();
        append_log(&self.proc.log_path(), "launch: start\n");
        self.proc.spawn(false)?;
        // The fault (if any) is armed for this process only.
        self.proc.fault = None;
        if let Err(error) = self.proc.wait_ready(&self.tiger) {
            let log = self.log_since_start();
            if armed && log.contains("tiger-fault:") {
                // The fault fired while reopening: reopen unarmed.
                self.event(
                    "faultCrashOnOpen",
                    json!(log.lines().find(|l| l.contains("tiger-fault:"))),
                );
                return self.recover(false);
            }
            self.violation(
                "reopen-failed",
                json!(format!("{error:#}")),
                json!("a node that answers"),
                json!(self.log_tail()),
            );
            bail!("tiger did not reopen: {error:#}");
        }
        self.open_epoch(clean.then_some(self.head))
    }

    /// Reads the reopened head, checks it and the chain and state below it, and lets readers in.
    /// `expected`: the head that must have survived.
    fn open_epoch(&mut self, expected: Option<B256>) -> eyre::Result<()> {
        let rec = self.rec();
        let block = loop {
            match self.tiger.result("eth_getBlockByNumber", &json!(["latest", false])) {
                Ok(block) => break block,
                Err(error) => {
                    if !self.proc.alive() || !fault_window_open() {
                        self.handle_failure(&error.to_string())?;
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(300));
                }
            }
        };
        let Some(head) = recording::hash(&block["hash"]).filter(|h| rec.blocks.contains_key(h))
        else {
            self.violation("recovered-unknown", json!("latest"), json!("a recorded block"), block);
            bail!("tiger reopened at an unknown block");
        };
        match expected {
            Some(expected) if expected != head => self.violation(
                "recovered-head",
                json!("latest after a clean stop"),
                json!({ "hash": expected, "number": rec.blocks[&expected].number }),
                json!({ "hash": head, "number": rec.blocks[&head].number }),
            ),
            None if !self.heads_seen.iter().any(|h| rec.is_canonical_under(head, *h)) => self
                .violation(
                    "recovered-head",
                    json!("latest after a crash"),
                    json!("a block canonical under a head tiger had"),
                    json!({ "hash": head, "number": rec.blocks[&head].number }),
                ),
            _ => {}
        }
        self.check_chain(head);
        self.head = head;
        self.heads_seen.insert(head);
        let epoch = {
            let mut timeline = self.shared.timeline.lock().expect("timeline");
            timeline.epochs.push(Epoch { recovered: head, opened: self.shared.now() });
            timeline.epochs.len() as u64 - 1
        };
        self.event(
            "epoch",
            json!({ "epoch": epoch, "head": head, "number": rec.blocks[&head].number }),
        );
        self.shared.epoch.store(epoch, Ordering::SeqCst);
        self.shared.open.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Checks the canonical chain below `head` (block hashes by number) and the state at the head
    /// and a few ancestors (proof roots and proofs) against the recording.
    fn check_chain(&mut self, head: B256) {
        let rec = self.rec();
        let top = rec.blocks[&head].number;
        let mut heights: Vec<u64> = if top <= 160 {
            (0..=top).collect()
        } else {
            let mut h: Vec<u64> = (top - 16..=top).collect();
            h.extend((0..48).map(|_| self.rng.below(top)));
            h
        };
        heights.sort_unstable();
        heights.dedup();
        for number in heights {
            let expected = rec.ancestor_at(head, number);
            match self.tiger.result("eth_getBlockByNumber", &json!([format!("{number:#x}"), false]))
            {
                Ok(block) if recording::hash(&block["hash"]) == expected => {}
                Ok(block) => self.violation(
                    "recovered-chain",
                    json!(number),
                    json!(expected),
                    block["hash"].clone(),
                ),
                Err(error) => self.violation(
                    "recovered-chain",
                    json!(number),
                    json!(expected),
                    json!(error.to_string()),
                ),
            }
        }
        let mut states = vec![head];
        for _ in 0..3 {
            let number = self.rng.below(top + 1);
            states.extend(rec.ancestor_at(head, number));
        }
        for at in states {
            if self.shared.quirks.contains(&at) {
                continue;
            }
            let block = &rec.blocks[&at];
            for (address, slots) in rec.keys_near(at, 6) {
                let request = json!([address, slots, format!("{:#x}", block.number)]);
                match self.tiger.result("eth_getProof", &request) {
                    Ok(proof) => match proof_root(&proof) {
                        Ok(root) if root == block.state_root => {
                            if let Err(error) = verify_account_proof(address, &slots, root, &proof)
                            {
                                self.violation(
                                    "recovered-proof",
                                    request,
                                    json!(block.state_root),
                                    json!(error),
                                );
                            }
                        }
                        other => self.violation(
                            "recovered-state-root",
                            request,
                            json!(block.state_root),
                            json!(other.ok()),
                        ),
                    },
                    Err(error) => self.violation(
                        "recovered-proof",
                        request,
                        json!(block.state_root),
                        json!(error.to_string()),
                    ),
                }
            }
        }
    }

    /// base-reth opens a copy of the datadir tiger crashed on, as an operator switching clients
    /// after a crash would; it must reopen on a block tiger had as canonical, with intact state.
    fn crash_handoff(&mut self) -> eyre::Result<()> {
        let rec = self.rec();
        let handoff = self.dir.join("handoff");
        copy_dir(&self.proc.datadir(), &handoff)?;
        let _ = fs::remove_file(handoff.join("discovery-secret"));
        let mut proc =
            NodeProcess::new("handoff".into(), self.config.launch.clone(), self.dir.clone());
        let node = NodeClient::parse(
            &Self::handoff_spec(self.config.port_offset),
            &self.dir.join("jwt.hex"),
        )?;
        proc.spawn(false)?;
        let result = proc.wait_ready(&node);
        self.event("crashHandoff", json!(null));
        if let Err(error) = result {
            proc.stop(true);
            let tail = tail_of(&proc.log_path(), 30);
            self.violation(
                "crash-handoff-open",
                json!("base-reth opens tiger's crashed datadir"),
                json!("it opens"),
                json!({ "error": format!("{error:#}"), "log": tail }),
            );
            return Ok(());
        }
        match node.result("eth_getBlockByNumber", &json!(["latest", false])) {
            Ok(block) => {
                let head = recording::hash(&block["hash"]).unwrap_or_default();
                if !self.heads_seen.iter().any(|h| rec.is_canonical_under(head, *h)) {
                    self.violation(
                        "crash-handoff-head",
                        json!("base-reth latest"),
                        json!("a block canonical under a head tiger had"),
                        block,
                    );
                } else {
                    let number = rec.blocks[&head].number;
                    for (address, slots) in rec.keys_near(head, 4) {
                        let request = json!([address, slots, format!("{number:#x}")]);
                        match node.result("eth_getProof", &request).map(|p| proof_root(&p)) {
                            Ok(Ok(root)) if root == rec.blocks[&head].state_root => {}
                            other => self.violation(
                                "crash-handoff-state",
                                request,
                                json!(rec.blocks[&head].state_root),
                                json!(format!("{other:?}")),
                            ),
                        }
                    }
                }
            }
            Err(error) => self.violation(
                "crash-handoff-head",
                json!("latest"),
                json!("a block"),
                json!(error.to_string()),
            ),
        }
        let log = fs::read_to_string(proc.log_path()).unwrap_or_default();
        let clean = proc.stop(false);
        let warnings: Vec<&str> = log
            .lines()
            .filter(|l| l.contains(" WARN ") || l.contains(" ERROR "))
            .take(10)
            .collect();
        if !warnings.is_empty() || !clean {
            self.event("crashHandoffWarnings", json!(warnings));
        }
        fs::remove_dir_all(&handoff)?;
        Ok(())
    }

    /// End of a run: tiger moves to the final head, stops cleanly and must reopen there; its
    /// chain must match base-reth's answers; then base-reth opens a copy of tiger's datadir and
    /// must answer the same.
    fn finish(&mut self) -> eyre::Result<()> {
        self.settle()?;
        let rec = self.rec();
        let head = rec.final_head();
        let last = rec.steps.iter().rev().find_map(|s| match s.kind {
            StepKind::Forkchoice { safe, finalized, .. } => Some((safe, finalized)),
            _ => None,
        });
        let (safe, finalized) = last.unwrap_or((rec.genesis, rec.genesis));
        self.forkchoice(head, safe, finalized)?;
        if let Some(killer) = self.killer.take() {
            let _ = killer.join();
            // A late kill: recover before the final checks.
            if !self.proc.alive() {
                self.recover(false)?;
                self.forkchoice(head, safe, finalized)?;
            }
        }
        self.close();
        let clean = self.proc.stop(false);
        if !clean || !self.log_since_start().contains("persisted every block through the head") {
            self.violation(
                "final-stop",
                json!("SIGTERM"),
                json!("clean stop after persisting"),
                json!(self.log_tail()),
            );
        }
        self.recover(true)?;
        self.close();
        let mismatches = Self::compare_with_oracle(&rec, &self.tiger, head);
        for (kind, request, expected, got) in mismatches {
            self.violation(&format!("oracle-{kind}"), request, expected, got);
        }
        self.proc.stop(false);
        // base-reth opens what tiger left.
        let handoff = self.dir.join("handoff");
        copy_dir(&self.proc.datadir(), &handoff)?;
        let _ = fs::remove_file(handoff.join("discovery-secret"));
        let mut proc =
            NodeProcess::new("handoff".into(), self.config.launch.clone(), self.dir.clone());
        let node = NodeClient::parse(
            &Self::handoff_spec(self.config.port_offset),
            &self.dir.join("jwt.hex"),
        )?;
        proc.spawn(false)?;
        if let Err(error) = proc.wait_ready(&node) {
            proc.stop(true);
            self.violation(
                "handoff-open",
                json!("base-reth opens tiger's datadir"),
                json!("it opens"),
                json!({ "error": format!("{error:#}"), "log": tail_of(&proc.log_path(), 30) }),
            );
            return Ok(());
        }
        self.event("handoff", json!(null));
        let mismatches = Self::compare_with_oracle(&rec, &node, head);
        for (kind, request, expected, got) in mismatches {
            self.violation(&format!("handoff-{kind}"), request, expected, got);
        }
        let log = fs::read_to_string(proc.log_path()).unwrap_or_default();
        let warnings: Vec<String> = log
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                (l.contains("warn") || l.contains("error"))
                    && (l.contains("heal")
                        || l.contains("unwind")
                        || l.contains("inconsisten")
                        || l.contains("rebuild"))
            })
            .take(10)
            .map(str::to_owned)
            .collect();
        if !warnings.is_empty() {
            self.violation("handoff-warnings", json!("base-reth log"), json!([]), json!(warnings));
        }
        proc.stop(false);
        fs::remove_dir_all(&handoff).wrap_err("removing the handoff datadir")?;
        Ok(())
    }

    /// Compares `node`'s chain ending at `head` with base-reth's recorded answers: every block and
    /// its receipts by number, and proofs at a few heights. Returns mismatches.
    fn compare_with_oracle(
        rec: &Recording,
        node: &NodeClient,
        head: B256,
    ) -> Vec<(String, Value, Value, Value)> {
        let mut mismatches = Vec::new();
        match node.result("eth_getBlockByNumber", &json!(["latest", false])) {
            Ok(block) if recording::hash(&block["hash"]) == Some(head) => {}
            Ok(block) => mismatches.push((
                "head".into(),
                json!("latest"),
                json!(head),
                block["hash"].clone(),
            )),
            Err(error) => mismatches.push((
                "head".into(),
                json!("latest"),
                json!(head),
                json!(error.to_string()),
            )),
        }
        for (number, oracle) in &rec.oracle {
            if mismatches.len() >= 20 {
                break;
            }
            let tag = format!("{number:#x}");
            match node.result("eth_getBlockByNumber", &json!([tag, false])) {
                Ok(block) if block == oracle.block => {}
                Ok(block) => {
                    mismatches.push(("block".into(), json!(number), oracle.block.clone(), block))
                }
                Err(error) => mismatches.push((
                    "block".into(),
                    json!(number),
                    oracle.block.clone(),
                    json!(error.to_string()),
                )),
            }
            match node.result("eth_getBlockReceipts", &json!([tag])) {
                Ok(receipts) if receipts == oracle.receipts => {}
                Ok(receipts) => mismatches.push((
                    "receipts".into(),
                    json!(number),
                    oracle.receipts.clone(),
                    receipts,
                )),
                Err(error) => mismatches.push((
                    "receipts".into(),
                    json!(number),
                    json!("receipts"),
                    json!(error.to_string()),
                )),
            }
        }
        let top = rec.blocks[&head].number;
        for number in [top, top.saturating_sub(1), top / 2, top.saturating_sub(40)] {
            let Some(at) = rec.ancestor_at(head, number) else { continue };
            let block = &rec.blocks[&at];
            for (address, slots) in rec.keys_near(at, 8) {
                let request = json!([address, slots, format!("{number:#x}")]);
                match node.result("eth_getProof", &request) {
                    Ok(proof) => {
                        let ok = proof_root(&proof).is_ok_and(|r| r == block.state_root)
                            && verify_account_proof(address, &slots, block.state_root, &proof)
                                .is_ok();
                        if !ok {
                            mismatches.push((
                                "proof".into(),
                                request,
                                json!(block.state_root),
                                proof,
                            ));
                        }
                    }
                    Err(error) => mismatches.push((
                        "proof".into(),
                        request,
                        json!(block.state_root),
                        json!(error.to_string()),
                    )),
                }
            }
        }
        mismatches
    }
}

fn append_log(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn tail_of(path: &Path, lines: usize) -> Vec<String> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].iter().map(|l| (*l).to_owned()).collect()
}
