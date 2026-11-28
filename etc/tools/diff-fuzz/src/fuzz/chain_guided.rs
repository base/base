//! Coverage-guided chain fuzzing: chain prefixes whose blocks reach new coverage are kept as
//! generator snapshots, and later runs continue them with edits.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    Adapter, AdapterSpec, COVERAGE_ENV, Campaign, ChainDriver, ChainEdit, ChainGenerator, Corpus,
    CorpusCase, Era, FindingGroup, FindingSignature, Fuzzer, Minimizer, MutationStats, Request,
    Rng,
};

/// A kept chain prefix: chain `seed` with `edits` applied to the blocks at their indices, run for
/// `blocks` blocks. The prefix is a deterministic function of this descriptor and the agreed
/// responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainCase {
    /// Chain seed.
    pub seed: u64,
    /// Edits by the index of the block they changed.
    pub edits: Vec<(u64, ChainEdit)>,
    /// Blocks in the prefix.
    pub blocks: u64,
}

impl CorpusCase for ChainCase {
    fn id(&self) -> String {
        format!("chain-{:x}-e{}-b{}", self.seed, self.edits.len(), self.blocks)
    }
}

/// Counters of a coverage-guided chain run.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainFuzzStats {
    /// Runs: fresh chains plus continuations of kept prefixes.
    pub runs: u64,
    /// Of those, fresh chains.
    pub fresh: u64,
    /// Blocks executed.
    pub blocks: u64,
    /// Blocks every adapter agreed on.
    pub agreed_blocks: u64,
    /// Runs that ended in a divergence.
    pub diverge: u64,
    /// Runs that ended in an adapter failure.
    pub adapter_failure: u64,
    /// Runs the generator could not continue.
    pub stopped: u64,
    /// Blocks no coverage adapter returned coverage for.
    pub no_coverage: u64,
    /// Config update edits that could not apply (the L1 origin could not advance).
    pub skipped_edits: u64,
    /// Longest chain, in blocks from its genesis.
    pub longest: u64,
    /// Agreed blocks by the era they executed in.
    pub agreed_by_era: BTreeMap<Era, u64>,
}

/// One point of the coverage timeline.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainSample {
    /// Seconds since the start.
    pub secs: u64,
    /// Blocks executed.
    pub blocks: u64,
    /// Distinct features (name and hit-count bucket).
    pub features: u64,
    /// Distinct feature names.
    pub names: u64,
    /// Distinct edges per client.
    pub edges: BTreeMap<String, u64>,
    /// Kept prefixes.
    pub corpus: u64,
    /// Runs that ended in a finding.
    pub findings: u64,
}

/// One run of a worker: a fresh chain or a kept prefix with edits scheduled on it.
#[derive(Debug, Clone)]
pub struct ChainRun {
    /// The chain, at its first block to run.
    pub chain: ChainGenerator,
    /// Its descriptor so far.
    pub case: ChainCase,
    /// Edit kinds (see [`ChainFuzzer::edit_kind`]) by the block index they apply to.
    pub schedule: Vec<(u64, u8)>,
    /// Blocks to run.
    pub budget: u64,
    /// Mutation operators the run applies, for the operator statistics.
    pub ops: Vec<&'static str>,
}

/// State the workers share.
#[derive(Debug)]
pub struct ChainFuzzState {
    /// Kept prefixes and feature statistics.
    pub corpus: Corpus<ChainCase>,
    /// The generator after each kept prefix, by corpus index; low-energy ones are dropped to
    /// bound memory.
    pub snapshots: Vec<Option<Arc<ChainGenerator>>>,
    /// Snapshots held.
    pub live: usize,
    /// Index of the next fresh chain.
    pub next_chain: u64,
    /// Counters.
    pub stats: ChainFuzzStats,
    /// Findings by signature key.
    pub findings: BTreeMap<(bool, std::collections::BTreeSet<String>, String), FindingGroup>,
    /// Mutation operator statistics.
    pub mutations: BTreeMap<&'static str, MutationStats>,
    /// Coverage timeline.
    pub timeline: Vec<ChainSample>,
    /// When the report was last written.
    pub last_report: Instant,
    /// When the timeline was last sampled.
    pub last_sample: Instant,
}

/// Coverage-guided chain fuzzing. Every block's coverage from the `coverage_from` adapters is
/// merged; a block that reaches a new feature keeps its chain prefix, as a generator snapshot
/// after the block. Guided runs continue a prefix picked by energy with one to three edits: a
/// re-planned block (`reroll`), a system config update (`configUpdate`), a timestamp gap
/// (`gap`), each sometimes applied a few blocks after the prefix (`fork`). The rest, and every
/// unguided run, are fresh chains.
#[derive(Debug, Clone)]
pub struct ChainFuzzer {
    /// Adapters, reference, workers, output, seed, generator options, and minimization budget;
    /// `chains` and `length` are unused.
    pub driver: ChainDriver,
    /// Adapters started with coverage on.
    pub coverage_from: Vec<String>,
    /// How long to run.
    pub duration: Duration,
    /// Continue kept prefixes (`true`) or only run fresh chains (`false`, the baseline, which
    /// still measures coverage).
    pub guided: bool,
    /// Percentage of guided runs that start a fresh chain instead.
    pub explore_percent: u64,
    /// Lengths of fresh chains, picked uniformly.
    pub lengths: Vec<u64>,
    /// Most snapshots held.
    pub max_snapshots: usize,
    /// How often to add a sample to the timeline.
    pub sample_every: Duration,
    /// Count a new hit-count bucket of a known feature name as new coverage. With merged
    /// coverage of several clients nearly every block reaches some new bucket, which keeps most
    /// blocks and flattens the energy signal; without buckets only new names count.
    pub buckets: bool,
}

impl ChainFuzzer {
    /// Findings recorded per signature.
    pub const RECORD_PER_SIGNATURE: u64 = 5;
    /// Findings minimized per signature.
    pub const MINIMIZE_PER_SIGNATURE: u64 = 2;

    /// Runs until the duration elapses and returns the counters. `chain-coverage.json` in the
    /// output directory is rewritten every ten seconds.
    pub fn run(&self) -> eyre::Result<ChainFuzzStats> {
        let driver = &self.driver;
        for name in std::iter::once(&driver.reference).chain(&self.coverage_from) {
            eyre::ensure!(
                driver.adapters.iter().any(|a| &a.name == name),
                "adapter {name} is not among the adapters"
            );
        }
        eyre::ensure!(!self.lengths.is_empty(), "no fresh chain lengths");
        fs::create_dir_all(driver.out.join("findings"))?;
        let started = Instant::now();
        let state = Mutex::new(ChainFuzzState {
            corpus: Corpus::default(),
            snapshots: Vec::new(),
            live: 0,
            next_chain: 0,
            stats: ChainFuzzStats::default(),
            findings: BTreeMap::new(),
            mutations: BTreeMap::new(),
            timeline: Vec::new(),
            last_report: started,
            last_sample: started,
        });
        let specs: Vec<AdapterSpec> = driver
            .adapters
            .iter()
            .map(|spec| {
                let mut spec = spec.clone();
                if self.coverage_from.contains(&spec.name) {
                    let mut command = vec!["env".to_owned(), format!("{COVERAGE_ENV}=1")];
                    command.append(&mut spec.command);
                    spec.command = command;
                }
                spec
            })
            .collect();
        thread::scope(|scope| {
            for worker in 0..driver.jobs.max(1) {
                let (state, specs) = (&state, &specs);
                scope.spawn(move || self.worker(worker as u64, state, specs, started));
            }
        });
        let mut state = state.into_inner().unwrap();
        Self::sample(&mut state, started);
        self.report(&state, started)?;
        Ok(state.stats)
    }

    fn worker(
        &self,
        worker: u64,
        state: &Mutex<ChainFuzzState>,
        specs: &[AdapterSpec],
        started: Instant,
    ) {
        let driver = &self.driver;
        let mut adapters: Vec<Adapter> =
            specs.iter().map(|spec| Adapter::new(spec.clone(), driver.timeout)).collect();
        let reference = adapters
            .iter()
            .position(|a| a.spec.name == driver.reference)
            .expect("run checks the reference");
        let covered: Vec<usize> = adapters
            .iter()
            .enumerate()
            .filter(|(_, a)| self.coverage_from.contains(&a.spec.name))
            .map(|(i, _)| i)
            .collect();
        let campaign = Campaign {
            adapters: driver.adapters.clone(),
            jobs: driver.jobs,
            timeout: driver.timeout,
            out: driver.out.clone(),
            max_findings: u64::MAX,
        };
        let mut rng = Rng::new(&[driver.seed, worker, 0xc4a1_0001]);
        while started.elapsed() < self.duration {
            let Some(mut run) = self.next_run(state, &mut rng) else { continue };
            let first = run.chain.blocks();
            let mut kept = false;
            let mut prefix: VecDeque<Request> = VecDeque::new();
            while run.chain.blocks() < first + run.budget && started.elapsed() < self.duration {
                let at = run.chain.blocks();
                let due: Vec<u8> =
                    run.schedule.iter().filter(|(b, _)| *b == at).map(|(_, k)| *k).collect();
                for kind in due {
                    let edit = Self::edit_kind(kind, &mut rng, &run.chain);
                    match catch_unwind(AssertUnwindSafe(|| run.chain.edit(edit))) {
                        Ok(true) => run.case.edits.push((at, edit)),
                        Ok(false) => state.lock().unwrap().stats.skipped_edits += 1,
                        Err(_) => {
                            state.lock().unwrap().stats.stopped += 1;
                            run.budget = 0;
                        }
                    }
                }
                if run.budget == 0 {
                    break;
                }
                let request = run.chain.request().clone();
                if prefix.len() == ChainDriver::PREFIX_LEN {
                    prefix.pop_front();
                }
                prefix.push_back(request.clone());
                let (results, verdict) = Minimizer::run(&mut adapters, &request);
                let mut coverage: Option<BTreeMap<String, u64>> = None;
                for response in covered.iter().filter_map(|&i| results[i].1.as_ref().ok()) {
                    if let Some(features) = &response.coverage {
                        coverage.get_or_insert_default().extend(features.clone());
                    }
                }
                if !self.buckets
                    && let Some(coverage) = &mut coverage
                {
                    coverage.values_mut().for_each(|count| *count = 1);
                }
                let signature = FindingSignature::of(&verdict, &driver.reference);
                let (features, novel, record) = {
                    let mut s = state.lock().unwrap();
                    s.stats.blocks += 1;
                    let (features, novel) = match &coverage {
                        Some(coverage) => s.corpus.observe(coverage),
                        None => {
                            s.stats.no_coverage += 1;
                            (Vec::new(), 0)
                        }
                    };
                    let record = signature.as_ref().map(|signature| {
                        if signature.failure {
                            s.stats.adapter_failure += 1;
                        } else {
                            s.stats.diverge += 1;
                        }
                        let group = s.findings.entry(signature.key()).or_default();
                        group.signature = signature.describe();
                        group.count += 1;
                        if group.count == 1 {
                            eprintln!(
                                "[{}] new finding {}: {}",
                                request.id,
                                group.signature,
                                verdict.describe()
                            );
                        }
                        group.count
                    });
                    (features, novel, record)
                };
                if let (Some(signature), Some(count)) = (signature, record) {
                    if count <= Self::RECORD_PER_SIGNATURE {
                        let results: Vec<_> = results
                            .iter()
                            .map(|(name, result)| {
                                (
                                    name.clone(),
                                    result
                                        .as_ref()
                                        .map(Fuzzer::without_coverage)
                                        .map_err(Clone::clone),
                                )
                            })
                            .collect();
                        let recorder = ChainDriver {
                            minimize_budget: if count <= Self::MINIMIZE_PER_SIGNATURE {
                                driver.minimize_budget
                            } else {
                                0
                            },
                            ..driver.clone()
                        };
                        let dir = recorder.record(
                            prefix.make_contiguous(),
                            &results,
                            &verdict,
                            &signature,
                            &mut adapters,
                            &campaign,
                        );
                        let _ = fs::write(
                            std::path::Path::new(&dir).join("case.json"),
                            serde_json::to_vec_pretty(&run.case).unwrap_or_default(),
                        );
                        let mut s = state.lock().unwrap();
                        if let Some(group) = s.findings.get_mut(&signature.key()) {
                            group.recorded.push(dir);
                        }
                    }
                    break;
                }
                let Ok(response) = &results[reference].1 else {
                    unreachable!("agreement needs responses")
                };
                let next = catch_unwind(AssertUnwindSafe(|| run.chain.next(response)));
                let snapshot = (self.guided && novel > 0 && matches!(next, Ok(Ok(_))))
                    .then(|| Arc::new(run.chain.clone()));
                let mut s = state.lock().unwrap();
                s.stats.agreed_blocks += 1;
                *s.stats.agreed_by_era.entry(Era::at(request.header.timestamp)).or_default() += 1;
                if !matches!(next, Ok(Ok(_))) {
                    s.stats.stopped += 1;
                    break;
                }
                run.case.blocks = run.chain.blocks();
                s.stats.longest = s.stats.longest.max(run.case.blocks);
                if novel > 0 {
                    kept = true;
                    // Only entries with a snapshot can be picked, so only they keep their
                    // feature list (thousands of ids per block); global hit counts stay exact.
                    let features = if snapshot.is_some() { features } else { Vec::new() };
                    let _ = s.corpus.add(run.case.clone(), features);
                    if snapshot.is_some() {
                        s.live += 1;
                    }
                    s.snapshots.push(snapshot);
                    if s.live > self.max_snapshots {
                        Self::evict(&mut s);
                    }
                }
                if s.last_sample.elapsed() >= self.sample_every {
                    Self::sample(&mut s, started);
                }
                if s.last_report.elapsed() >= Duration::from_secs(10) {
                    s.last_report = Instant::now();
                    if let Err(err) = self.report(&s, started) {
                        eprintln!("writing chain coverage report: {err:#}");
                    }
                }
            }
            let mut s = state.lock().unwrap();
            for op in &run.ops {
                let entry = s.mutations.entry(op).or_default();
                entry.tried += 1;
                entry.kept += u64::from(kept);
            }
        }
    }

    /// The next run: a continuation of a kept prefix with edits scheduled on it, or a fresh
    /// chain. `None` when a fresh chain's generator panicked.
    fn next_run(&self, state: &Mutex<ChainFuzzState>, rng: &mut Rng) -> Option<ChainRun> {
        let driver = &self.driver;
        let mut s = state.lock().unwrap();
        s.stats.runs += 1;
        let mut parent = None;
        if self.guided && s.live > 0 && !rng.chance(self.explore_percent) {
            for _ in 0..16 {
                let index = s.corpus.pick(rng).expect("snapshots imply entries");
                if let Some(snapshot) = &s.snapshots[index] {
                    parent = Some((Arc::clone(snapshot), s.corpus.entries[index].case.clone()));
                    break;
                }
            }
        }
        if let Some((snapshot, case)) = parent {
            drop(s);
            let chain = (*snapshot).clone();
            let mut schedule = Vec::new();
            let mut ops = Vec::new();
            let mut last = 0;
            for _ in 0..rng.range(1, 3) {
                let offset = if rng.chance(30) { rng.range(1, 16) } else { 0 };
                let kind = rng.below(3) as u8;
                schedule.push((chain.blocks() + offset, kind));
                ops.push(["reroll", "configUpdate", "gap"][kind as usize]);
                if offset > 0 {
                    ops.push("fork");
                }
                last = last.max(offset);
            }
            ops.sort_unstable();
            ops.dedup();
            return Some(ChainRun { chain, case, schedule, budget: last + rng.range(16, 96), ops });
        }
        let index = s.next_chain;
        s.next_chain += 1;
        s.stats.fresh += 1;
        drop(s);
        let seed = driver.seed.wrapping_mul(1_000_000).wrapping_add(index);
        let mut options = driver.options.clone();
        if self.guided {
            // Guided runs start few fresh chains, so they rotate the era a fresh chain targets
            // (before its activation or within it) instead of weighting recent eras: every
            // era, Regolith included, gets an equal share.
            let eras = Era::targeted();
            options.eras = vec![eras[index as usize % eras.len()]];
        }
        let chain = catch_unwind(AssertUnwindSafe(|| ChainGenerator::new(seed, options))).ok()?;
        Some(ChainRun {
            chain,
            case: ChainCase { seed, edits: Vec::new(), blocks: 0 },
            schedule: Vec::new(),
            budget: *rng.pick(&self.lengths),
            ops: Vec::new(),
        })
    }

    /// Turns a scheduled edit kind (0 reroll, 1 config update, 2 gap) into an edit of `chain`'s
    /// pending block. A gap is a few seconds, an epoch or longer, or lands the block on (or one
    /// second after) the next upgrade activation.
    pub fn edit_kind(kind: u8, rng: &mut Rng, chain: &ChainGenerator) -> ChainEdit {
        match kind {
            0 => ChainEdit::Reroll { salt: rng.next_u64() },
            1 => ChainEdit::ConfigUpdate { salt: rng.next_u64() },
            _ => {
                let timestamp = chain.request().header.timestamp;
                let next = Era::all()
                    .iter()
                    .copied()
                    .map(Era::activation_timestamp)
                    .filter(|t| *t > timestamp)
                    .min();
                let secs = match (rng.below(3), next) {
                    (0, Some(activation)) => activation - timestamp + rng.below(2),
                    _ => *rng.pick(&[1, 2, 3, 12, 13, 60, 600, 3_600, 86_400]),
                };
                ChainEdit::Gap { secs }
            }
        }
    }

    /// Drops the lowest-energy tenth of the snapshots. Energies sum over thousands of features
    /// per entry, so eviction works in batches rather than once per kept block.
    fn evict(s: &mut ChainFuzzState) {
        let mut held: Vec<(f64, usize)> = (0..s.snapshots.len())
            .filter(|&i| s.snapshots[i].is_some())
            .map(|i| (s.corpus.energy(&s.corpus.entries[i]), i))
            .collect();
        held.sort_by(|a, b| a.0.total_cmp(&b.0));
        for &(_, index) in held.iter().take(held.len().div_ceil(10)) {
            s.snapshots[index] = None;
            s.corpus.entries[index].features = Vec::new();
            s.live -= 1;
        }
    }

    /// Distinct edge names per client: base's `edge:<i>`, and `<client>:edge:<i>` for others.
    pub fn edges(corpus: &Corpus<ChainCase>) -> BTreeMap<String, u64> {
        let mut edges = BTreeMap::new();
        for name in &corpus.names {
            let client = if name.starts_with("edge:") {
                "base"
            } else {
                match name.split_once(":edge:") {
                    Some((client, _)) => client,
                    None => continue,
                }
            };
            *edges.entry(client.to_owned()).or_default() += 1;
        }
        edges
    }

    fn sample(s: &mut ChainFuzzState, started: Instant) {
        s.last_sample = Instant::now();
        let sample = ChainSample {
            secs: started.elapsed().as_secs(),
            blocks: s.stats.blocks,
            features: s.corpus.ids.len() as u64,
            names: s.corpus.names.len() as u64,
            edges: Self::edges(&s.corpus),
            corpus: s.corpus.entries.len() as u64,
            findings: s.stats.diverge + s.stats.adapter_failure,
        };
        eprintln!(
            "[{}s] blocks {} features {} names {} edges {:?} corpus {} snapshots {} findings {}",
            sample.secs,
            sample.blocks,
            sample.features,
            sample.names,
            sample.edges,
            sample.corpus,
            s.live,
            sample.findings
        );
        s.timeline.push(sample);
    }

    fn report(&self, s: &ChainFuzzState, started: Instant) -> eyre::Result<()> {
        let elapsed = started.elapsed().as_secs_f64();
        let mut findings: Vec<&FindingGroup> = s.findings.values().collect();
        findings.sort_by_key(|group| std::cmp::Reverse(group.count));
        let report = serde_json::json!({
            "mode": if self.guided { "guided" } else { "unguided" },
            "elapsedSecs": elapsed,
            "blocksPerSec": s.stats.blocks as f64 / elapsed.max(1e-9),
            "stats": &s.stats,
            "features": s.corpus.ids.len(),
            "names": s.corpus.names.len(),
            // Native precompile features (`np:<precompile>:<selector>:<outcome>`) reached, so a
            // run shows which methods and revert paths its chains exercised.
            "nativeNames": s.corpus.names.iter().filter(|n| n.starts_with("np:")).collect::<BTreeSet<_>>(),
            "edges": Self::edges(&s.corpus),
            "families": s.corpus.families(),
            "corpus": s.corpus.entries.len(),
            "snapshots": s.live,
            "findings": findings,
            "mutations": &s.mutations,
            "timeline": &s.timeline,
        });
        let path = self.driver.out.join("chain-coverage.json");
        let tmp = self.driver.out.join("chain-coverage.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&report)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}
