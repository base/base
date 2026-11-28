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
/// Failed calls on a full real disk before the ballast is removed.
const STUCK_RETRIES: u32 = 10;
/// How long tiger may keep failing after the ballast is removed.
const FREED_GRACE: Duration = Duration::from_secs(20);
/// How long disk errors are still expected after the ballast is removed.
const FREED_SETTLE: Duration = Duration::from_secs(10);

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
    /// Percentage of Engine API calls sent twice at once.
    pub duplicate_percent: u64,
    /// Percentage of runs that start with base-reth following the recording and being killed,
    /// so tiger opens base-reth's crashed datadir.
    pub base_crash_percent: u64,
    /// Percentage of runs with a stateless tiger.
    pub stateless_percent: u64,
    /// Percentage of runs whose datadir is a small tmpfs that chaos fills (real ENOSPC).
    pub real_disk_percent: u64,
    /// Percentage of restarts killed again while reopening.
    pub reopen_kill_percent: u64,
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
    /// The recording's latest forkchoice head (where tiger returns to after a flap).
    trace_head: B256,
    /// The recording's latest safe and finalized blocks.
    safe: B256,
    finalized: B256,
    /// tiger's safe and finalized blocks as the driver believes them; `None` after a reopen.
    tags: Option<(B256, B256)>,
    /// Every head tiger was moved to or reopened at.
    heads_seen: HashSet<B256>,
    /// Set before the driver kills tiger on purpose.
    expected_death: Arc<AtomicBool>,
    /// Kill thread in flight.
    killer: Option<thread::JoinHandle<()>>,
    events: BTreeMap<String, u64>,
    /// Log of driver actions, for reproducers.
    actions: Vec<Value>,
    /// tiger is about to reopen a datadir base-reth wrote and crashed on.
    base_written: bool,
    /// The lowest block tiger holds: a stateless node's start block, else genesis.
    floor: u64,
    /// The upstream of a stateless tiger (base-reth on the recording's final datadir).
    upstream: Option<NodeProcess>,
    /// tiger's datadir is a small tmpfs of its own, which `fillDisk` fills.
    real_disk: bool,
    /// The step at which the ballast filling the disk is removed.
    ballast_until: Option<usize>,
    /// The running process has a fault armed.
    armed: bool,
    /// Consecutive Engine API calls failed in a disk-error window.
    disk_retries: u32,
    /// When the ballast was removed because tiger could not take a step on the full disk.
    freed_at: Option<Instant>,
    /// When the ballast was last removed (disk errors are expected for a while after).
    disk_freed_at: Option<Instant>,
}

impl ChaosRun {
    /// tiger's HTTP and Engine API ports.
    fn tiger_spec(offset: u16) -> String {
        format!("tiger=http://127.0.0.1:{},http://127.0.0.1:{}", 28545 + offset, 28551 + offset)
    }

    /// The stateless upstream's ports (launch name `oracle`).
    fn oracle_spec(offset: u16) -> String {
        format!("oracle=http://127.0.0.1:{},http://127.0.0.1:{}", 20545 + offset, 20551 + offset)
    }

    /// A stateless run's recording: the final canonical chain from a start block above the last
    /// storage wipe (base-reth's witnesses below it are wrong, B1) and at least 60 blocks below
    /// the head, as a payload and a forkchoice update per block (safe 4 and finalized 32 blocks
    /// back, never below the start). Side branches are left out: the upstream holds only the
    /// final chain, so it cannot serve their witnesses. Returns the view and the start block.
    fn stateless_view(rec: &Recording, rng: &mut Rng) -> Option<(Recording, B256)> {
        let head = rec.final_head();
        let top = rec.blocks[&head].number;
        let last_wipe = (1..=top)
            .rev()
            .find(|n| rec.ancestor_at(head, *n).is_some_and(|h| rec.blocks[&h].wipes_storage))
            .unwrap_or(0);
        let lowest = last_wipe.max(1);
        if lowest + 60 > top {
            return None;
        }
        let start_number = rng.range(lowest, top - 60);
        let start = rec.ancestor_at(head, start_number)?;
        let mut view = rec.clone();
        view.steps.clear();
        view.snapshot = None;
        for number in start_number + 1..=top {
            let hash = rec.ancestor_at(head, number)?;
            let original = rec.steps[rec.blocks[&hash].step?].clone();
            view.blocks.get_mut(&hash)?.step = Some(view.steps.len());
            view.steps.push(original);
            let back = |n: u64| rec.ancestor_at(head, number.saturating_sub(n).max(start_number));
            let (safe, finalized) = (back(4)?, back(32)?);
            let state = json!({ "headBlockHash": hash, "safeBlockHash": safe, "finalizedBlockHash": finalized });
            view.steps.push(crate::Step {
                method: "engine_forkchoiceUpdatedV3".into(),
                params: json!([state, null]),
                kind: StepKind::Forkchoice { head: hash, safe, finalized },
            });
        }
        Some((view, start))
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
                "PORT_OFFSET={}\nTIGER_BIN={}\nBASE_NODE_BIN={}\nTIGER_EXTRA=\"{persistence}\"\nBASE_EXTRA=\"{persistence}\"\n",
                config.port_offset,
                config.tiger_bin.display(),
                config.base_bin.display()
            ),
        )?;
        let tiger = NodeClient::parse(&Self::tiger_spec(config.port_offset), &jwt)?;
        let proc = NodeProcess::new("tiger".into(), config.launch.clone(), dir.clone());
        let snapshot = recording.snapshot.clone().filter(|_| rng.chance(config.snapshot_percent));
        let base_crash = snapshot.is_none() && rng.chance(config.base_crash_percent);
        let stateless = (snapshot.is_none() && !base_crash && rng.chance(config.stateless_percent))
            .then(|| Self::stateless_view(recording, &mut rng))
            .flatten();
        let recording = &stateless
            .as_ref()
            .map_or_else(|| Arc::clone(recording), |(view, _)| Arc::new(view.clone()));
        let shared = Arc::new(Shared {
            start: Instant::now(),
            recording: Arc::clone(recording),
            timeline: Mutex::new(Timeline::default()),
            open: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            progress: AtomicUsize::new(snapshot.as_ref().map_or(0, |s| s.steps)),
            stop: AtomicBool::new(false),
            violations: Mutex::new(Vec::new()),
            quirks: std::sync::RwLock::new(if snapshot.is_some() {
                Self::base_reth_quirks(config, recording)?
            } else {
                HashSet::new()
            }),
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
            trace_head: recording.genesis,
            safe: recording.genesis,
            finalized: recording.genesis,
            tags: None,
            heads_seen: HashSet::from([recording.genesis]),
            expected_death: Arc::new(AtomicBool::new(false)),
            killer: None,
            events: BTreeMap::new(),
            actions: Vec::new(),
            base_written: false,
            floor: 0,
            upstream: None,
            real_disk: false,
            ballast_until: None,
            armed: false,
            disk_retries: 0,
            freed_at: None,
            disk_freed_at: None,
        };
        let mut report = RunReport {
            run,
            seed,
            start: if stateless.is_some() {
                "stateless".into()
            } else if snapshot.is_some() {
                "snapshot".into()
            } else if base_crash {
                "baseCrash".into()
            } else {
                "genesis".into()
            },
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
            this.trace_head = snapshot.head;
            this.safe = snapshot.safe;
            this.finalized = snapshot.finalized;
            this.heads_seen.insert(snapshot.head);
        }
        FAULT_WINDOW.store(false, Ordering::SeqCst);
        if snapshot.is_none() {
            let _ = fs::remove_dir_all(this.proc.datadir());
        }
        if base_crash {
            first = this.run_base_until_killed()?;
        }
        if let Some((_, start)) = &stateless {
            let start_block = &recording.blocks[start];
            let mut upstream =
                NodeProcess::new("oracle".into(), config.launch.clone(), dir.clone());
            copy_dir(&config.recording.join("oracle-datadir"), &upstream.datadir())?;
            let oracle = NodeClient::parse(&Self::oracle_spec(config.port_offset), &jwt)?;
            upstream.spawn(false)?;
            upstream.wait_ready(&oracle)?;
            this.upstream = Some(upstream);
            this.proc.stateless = Some(format!("{} {}", oracle.http_url, start_block.number));
            this.floor = start_block.number;
            this.head = *start;
            this.trace_head = *start;
            this.heads_seen.insert(*start);
            this.event("stateless", json!({ "start": start_block.number }));
        }
        fs::create_dir_all(this.proc.datadir())?;
        if snapshot.is_none() && !base_crash && this.rng.chance(config.real_disk_percent) {
            let status = std::process::Command::new("sudo")
                .args(["-n", "mount", "-t", "tmpfs", "-o", "size=256m", "tmpfs"])
                .arg(this.proc.datadir())
                .status()?;
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc::getuid() };
            let chown = std::process::Command::new("sudo")
                .args(["-n", "chown", &uid.to_string()])
                .arg(this.proc.datadir())
                .status()?;
            this.real_disk = status.success() && chown.success();
            this.event("realDisk", json!(this.real_disk));
        }
        append_log(&this.proc.log_path(), "launch: start\n");
        this.proc.spawn(false)?;
        this.proc.wait_ready(&this.tiger)?;
        let start_head = this.head;
        this.base_written = base_crash;
        this.open_epoch((!base_crash).then_some(start_head))?;
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
        if this.real_disk {
            if !this.shared.violations.lock().expect("violations").is_empty() {
                let _ = copy_dir(&this.proc.datadir(), &dir.join("tiger-kept"));
            }
            let _ = std::process::Command::new("sudo")
                .args(["-n", "umount"])
                .arg(this.proc.datadir())
                .status();
        }
        if let Some(mut upstream) = this.upstream.take() {
            upstream.stop(true);
            let _ = fs::remove_dir_all(upstream.datadir());
        }
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
        // B1: blocks below the highest canonical block (up to the snapshot head) that wiped a
        // storage-holding account.
        let top = rec.blocks[&snapshot.head].number;
        let last_wipe = (1..=top).rev().find(|n| {
            rec.ancestor_at(snapshot.head, *n).is_some_and(|h| rec.blocks[&h].wipes_storage)
        });
        let mut quirks = HashSet::new();
        let mut unexplained = Vec::new();
        for number in 0..=top {
            let Some(at) = rec.ancestor_at(snapshot.head, number) else { continue };
            let proof = node.result(
                "eth_getProof",
                &json!(["0x4200000000000000000000000000000000000015", [], format!("{number:#x}")]),
            )?;
            if proof_root(&proof).ok() != Some(rec.blocks[&at].state_root) {
                if last_wipe.is_some_and(|w| number < w) {
                    quirks.insert(at);
                } else {
                    unexplained.push(number);
                }
            }
        }
        if !unexplained.is_empty() {
            // Not B1: left unskipped, so the runs on this snapshot report them.
            eprintln!(
                "base-reth's own proofs miss the state root at blocks {unexplained:?}, which no storage wipe explains"
            );
        }
        proc.stop(false);
        let _ = fs::remove_dir_all(&dir);
        eprintln!(
            "base-reth's own proofs miss the state root at {} snapshot blocks below the storage wipe at {last_wipe:?} (B1)",
            quirks.len()
        );
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
            let released = self.ballast_until.is_some_and(|at| at <= index);
            if released {
                self.release_disk();
            }
            if !released && self.rng.below(1000) < per_mille {
                self.chaos_action()?;
            }
            self.run_step(index)?;
            self.disk_retries = 0;
            if self.freed_at.take().is_some() {
                self.disk_freed_at = Some(Instant::now());
            }
            if self.disk_freed_at.is_some_and(|at| at.elapsed() >= FREED_SETTLE) {
                // tiger took a step a while after space was freed: disk errors are no longer
                // expected (RocksDB retries a write that hit a full disk in the background).
                self.disk_freed_at = None;
                FAULT_WINDOW.store(false, Ordering::SeqCst);
            }
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

    /// base-reth (launch name `handoff`, same persistence flags) follows the recording from
    /// genesis and is killed at a random point, 0 to 300 ms into a step; its datadir becomes
    /// tiger's. Returns the first step tiger gets. Every head base-reth acknowledged counts as one
    /// the datadir may reopen at.
    fn run_base_until_killed(&mut self) -> eyre::Result<usize> {
        let rec = self.rec();
        let base = NodeClient::parse(
            &Self::handoff_spec(self.config.port_offset),
            &self.dir.join("jwt.hex"),
        )?;
        let mut proc =
            NodeProcess::new("handoff".into(), self.config.launch.clone(), self.dir.clone());
        proc.spawn(true)?;
        proc.wait_ready(&base)?;
        let kill_at = self.rng.range(1, rec.steps.len() as u64 - 1) as usize;
        let mut index = 0;
        let mut killer = None;
        while index < rec.steps.len() {
            if index == kill_at {
                let datadir = proc.datadir();
                let delay = Duration::from_millis(self.rng.below(300));
                killer = Some(thread::spawn(move || {
                    thread::sleep(delay);
                    for pid in Self::node_pids(&datadir) {
                        // SAFETY: kill has no memory-safety preconditions.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }));
            }
            let step = &rec.steps[index];
            let Ok(reply) = base.engine(&step.method, &step.params) else { break };
            if let StepKind::Forkchoice { head, safe, finalized } = step.kind
                && reply["payloadStatus"]["status"] == "VALID"
            {
                self.heads_seen.insert(head);
                self.trace_head = head;
                (self.safe, self.finalized) = (safe, finalized);
            }
            index += 1;
        }
        if let Some(killer) = killer {
            let _ = killer.join();
        }
        proc.stop(true);
        self.event("baseKilled", json!({ "step": index }));
        fs::rename(proc.datadir(), self.proc.datadir())?;
        let _ = fs::remove_file(self.proc.datadir().join("discovery-secret"));
        self.shared.progress.store(index, Ordering::SeqCst);
        Ok(index)
    }

    /// Fills tiger's datadir file system (a small tmpfs, `--real-disk-percent` of runs) with a
    /// ballast file, leaving at most a megabyte free, for 1 to 40 steps: every write path meets a
    /// real `ENOSPC`, the `RocksDB` WAL and the libmdbx file growth included.
    fn fill_disk(&mut self) -> eyre::Result<()> {
        let datadir = self.proc.datadir();
        let output = std::process::Command::new("df")
            .args(["--output=avail", "-B1"])
            .arg(&datadir)
            .output()?;
        let avail: u64 = String::from_utf8_lossy(&output.stdout)
            .lines()
            .nth(1)
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        let leave = *self.rng.pick(&[0u64, 4096, 65536, 1 << 20]);
        let steps = self.rng.range(1, 40) as usize;
        let ballast = datadir.join("ballast");
        let status = std::process::Command::new("fallocate")
            .args(["-l", &avail.saturating_sub(leave).max(4096).to_string()])
            .arg(&ballast)
            .status()?;
        self.event(
            "fillDisk",
            json!({ "avail": avail, "leave": leave, "steps": steps, "ok": status.success() }),
        );
        FAULT_WINDOW.store(true, Ordering::SeqCst);
        self.ballast_until = Some(self.shared.progress.load(Ordering::SeqCst) + steps);
        Ok(())
    }

    /// Removes the ballast; the window for disk errors closes after the first step tiger takes
    /// `FREED_SETTLE` later.
    fn release_disk(&mut self) {
        let _ = fs::remove_file(self.proc.datadir().join("ballast"));
        self.ballast_until = None;
        self.disk_freed_at = Some(Instant::now());
        self.event("releaseDisk", json!(null));
    }

    /// One random chaos action between steps.
    fn chaos_action(&mut self) -> eyre::Result<()> {
        self.settle()?;
        let fill = if self.real_disk && self.ballast_until.is_none() { 18 } else { 0 };
        match self.rng.weighted(&[16, 22, 8, 10, 24, 20, fill]) {
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
            5 => self.flap()?,
            _ => self.fill_disk()?,
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
        if !clean && !log.contains("tiger-fault:") && self.ballast_until.is_none() {
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
        // Back to the recording's head (tiger's own head may be lower after a crash, below the
        // finalized block).
        let back = self.trace_head;
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
                self.trace_head = head;
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
            let dup = self.duplicate(&step.method, &step.params);
            let result = self.tiger.engine(&step.method, &step.params);
            self.check_duplicate(dup, &["VALID", "ACCEPTED", "SYNCING"]);
            match result {
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

    /// In `--duplicate-percent` of Engine API calls, sends the same call again from another
    /// thread at the same time, as a consensus node retrying after its own timeout does.
    fn duplicate(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Option<thread::JoinHandle<(Value, eyre::Result<Value>)>> {
        if !self.rng.chance(self.config.duplicate_percent) {
            return None;
        }
        self.event("duplicate", json!(method));
        let node = self.tiger.clone();
        let (method, params) = (method.to_owned(), params.clone());
        Some(thread::spawn(move || {
            let result = node.engine(&method, &params);
            (json!({ "method": method, "params": params }), result)
        }))
    }

    /// A duplicate call must get one of `allowed` statuses, or no answer when tiger went down.
    fn check_duplicate(
        &self,
        dup: Option<thread::JoinHandle<(Value, eyre::Result<Value>)>>,
        allowed: &[&str],
    ) {
        let Some(handle) = dup else { return };
        let Ok((request, result)) = handle.join() else { return };
        let Ok(reply) = result else { return };
        let status = reply.get("payloadStatus").unwrap_or(&reply)["status"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !allowed.contains(&status.as_str()) {
            self.violation("duplicate", request["method"].clone(), json!(allowed), reply);
        }
    }

    /// Resends the blocks of the chain ending at `hash` that tiger does not have.
    fn ensure_chain(&mut self, hash: B256) -> eyre::Result<()> {
        let rec = self.rec();
        let mut missing = Vec::new();
        let mut at = hash;
        while at != rec.genesis && rec.blocks[&at].number > self.floor {
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
            // base-reth keeps its head, safe and finalized blocks on an update naming one of the
            // head's ancestors.
            let ancestor = head != self.head && rec.is_canonical_under(head, self.head);
            let (effective, tags) =
                if ancestor { (self.head, self.tags) } else { (head, Some((safe, finalized))) };
            let epoch = self.epoch();
            let index = {
                let mut timeline = self.shared.timeline.lock().expect("timeline");
                timeline.fcus.push(Fcu {
                    epoch,
                    sent: self.shared.now(),
                    acked: None,
                    head: effective,
                    safe: tags.map(|t| t.0),
                    finalized: tags.map(|t| t.1),
                });
                timeline.fcus.len() - 1
            };
            let state = json!({ "headBlockHash": head, "safeBlockHash": safe, "finalizedBlockHash": finalized });
            let params = json!([state, null]);
            let dup = self.duplicate("engine_forkchoiceUpdatedV3", &params);
            let result = self.tiger.engine("engine_forkchoiceUpdatedV3", &params);
            self.check_duplicate(dup, &["VALID", "SYNCING"]);
            match result {
                Ok(result) => match result["payloadStatus"]["status"].as_str() {
                    Some("VALID") => {
                        self.shared.timeline.lock().expect("timeline").fcus[index].acked =
                            Some(self.shared.now());
                        self.head = effective;
                        self.tags = tags;
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
                self.disk_retries += 1;
                // An operator frees space when the node is stuck on a full disk; tiger must then
                // take the step without a restart, as base-reth's next persistence does.
                if self.disk_retries >= STUCK_RETRIES && self.ballast_until.is_some() {
                    self.release_disk();
                    self.freed_at = Some(Instant::now());
                }
                if self.freed_at.is_some_and(|at| at.elapsed() > FREED_GRACE) {
                    self.freed_at = None;
                    let tail = self.log_tail();
                    self.violation(
                        "stuck-after-disk-freed",
                        json!(error),
                        json!("steps taken once space is free"),
                        json!(tail),
                    );
                    self.expected_death.store(true, Ordering::SeqCst);
                    self.close();
                    self.proc.stop(true);
                }
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
        // A reopened node retries nothing a full disk failed; it is still full under a ballast.
        if self.proc.fault.is_none() && self.ballast_until.is_none() {
            self.disk_freed_at = None;
            FAULT_WINDOW.store(false, Ordering::SeqCst);
        }
        if !clean && self.floor == 0 && self.rng.chance(self.config.crash_handoff_percent) {
            self.crash_handoff()?;
        }
        let armed = self.proc.fault.is_some();
        append_log(&self.proc.log_path(), "launch: start\n");
        self.proc.spawn(false)?;
        // Crash again while it reopens and heals.
        for _ in 0..3 {
            if armed || !self.rng.chance(self.config.reopen_kill_percent) {
                break;
            }
            let delay = self.rng.below(400);
            thread::sleep(Duration::from_millis(delay));
            self.proc.stop(true);
            self.event("killDuringReopen", json!({ "delayMs": delay }));
            append_log(&self.proc.log_path(), "launch: start\n");
            self.proc.spawn(false)?;
        }
        // The fault (if any) is armed for this process only.
        self.proc.fault = None;
        self.armed = armed;
        if let Err(error) = self.proc.wait_ready(&self.tiger) {
            let log = self.log_since_start();
            if self.ballast_until.is_some() {
                // A full disk can keep a node from opening; it must open once space is free.
                self.event("openFailedDiskFull", json!(log.lines().last()));
                self.release_disk();
                return self.recover(false);
            }
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
        if std::mem::take(&mut self.base_written) {
            // base-reth wrote the history through this head: B1 applies below its last storage
            // wipe.
            let top = rec.blocks[&head].number;
            let last_wipe = (1..=top)
                .rev()
                .find(|n| rec.ancestor_at(head, *n).is_some_and(|h| rec.blocks[&h].wipes_storage));
            let mut quirks = self.shared.quirks.write().expect("quirks");
            for number in 0..last_wipe.unwrap_or(0) {
                quirks.extend(rec.ancestor_at(head, number));
            }
        }
        self.check_chain(head);
        self.head = head;
        self.tags = None;
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
        let floor = self.floor.min(top);
        let mut heights: Vec<u64> = if top - floor <= 160 {
            (floor..=top).collect()
        } else {
            let mut h: Vec<u64> = (top - 16..=top).collect();
            h.extend((0..48).map(|_| self.rng.range(floor, top)));
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
            let number = self.rng.range(floor, top + 1);
            states.extend(rec.ancestor_at(head, number));
        }
        for at in states {
            if self.shared.quirks.read().expect("quirks").contains(&at) {
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
        if self.ballast_until.is_some() {
            self.release_disk();
        }
        self.settle()?;
        let rec = self.rec();
        let head = rec.final_head();
        let last = rec.steps.iter().rev().find_map(|s| match s.kind {
            StepKind::Forkchoice { safe, finalized, .. } => Some((safe, finalized)),
            _ => None,
        });
        let (safe, finalized) = last.unwrap_or((rec.genesis, rec.genesis));
        self.forkchoice(head, safe, finalized)?;
        if self.real_disk && self.proc.fault.is_none() {
            FAULT_WINDOW.store(false, Ordering::SeqCst);
        }
        if let Some(killer) = self.killer.take() {
            let _ = killer.join();
            // A late kill: recover before the final checks.
            if !self.proc.alive() {
                self.recover(false)?;
                self.forkchoice(head, safe, finalized)?;
            }
        }
        // An armed fault may fail the shutdown's persistence, which loses unpersisted blocks
        // legitimately: crash the armed process and stop an unarmed one.
        if self.armed {
            self.expected_death.store(true, Ordering::SeqCst);
            self.close();
            self.proc.stop(true);
            self.event("disarm", json!(null));
            self.recover(false)?;
            self.forkchoice(head, safe, finalized)?;
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
        let mismatches = Self::compare_with_oracle(&rec, &self.tiger, head, self.floor);
        for (kind, request, expected, got) in mismatches {
            self.violation(&format!("oracle-{kind}"), request, expected, got);
        }
        self.proc.stop(false);
        if self.floor > 0 {
            // A partial datadir is tiger-only.
            return Ok(());
        }
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
        let mismatches = Self::compare_with_oracle(&rec, &node, head, 0);
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
        floor: u64,
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
        for (number, oracle) in rec.oracle.range(floor + 1..) {
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
        for number in
            [top, top.saturating_sub(1), top / 2, top.saturating_sub(40)].map(|n| n.max(floor))
        {
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
