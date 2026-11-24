//! Coverage-guided request rounds: per-request edge coverage from the nodes' `fuzz_coverage`
//! method decides which requests the corpus keeps for mutation.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    sync::Mutex,
    thread,
    time::Instant,
};

use alloy_primitives::{hex, keccak256};
use serde::Serialize;

use crate::{
    Corpus, CorpusCase, MutationStats, Rng, WorldState,
    nodes::{
        ChainFollower, ChainIndex, NodeClient, Reply, RpcCampaign, RpcCase, RpcConfig, RpcGen,
        RpcMutator, RpcSummary,
    },
};

impl CorpusCase for RpcCase {
    fn id(&self) -> String {
        let hash = keccak256(self.body.as_bytes());
        format!("{}-{}", self.method, hex::encode(&hash[..5]))
    }
}

/// One point of the coverage timeline.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuideSample {
    /// Seconds since the start.
    pub secs: u64,
    /// Requests sent with coverage.
    pub requests: u64,
    /// Distinct features (node edge and hit-count bucket).
    pub features: u64,
    /// Distinct edges over all coverage nodes.
    pub edges: u64,
    /// Corpus size.
    pub corpus: u64,
}

/// Counters of the guided rounds.
#[derive(Debug, Default)]
pub struct GuideState {
    /// Kept requests and the features seen so far.
    pub corpus: Corpus<RpcCase>,
    /// Saved corpus requests still to run.
    pub queue: VecDeque<RpcCase>,
    /// Requests sent with coverage.
    pub requests: u64,
    /// Readings that failed (no `fuzz_coverage` answer).
    pub failed_readings: u64,
    /// Requests by origin: `fresh` or `mutant`.
    pub origins: BTreeMap<String, MutationStats>,
    /// Per mutation operator, children tried and kept.
    pub mutations: BTreeMap<String, MutationStats>,
    /// Per method, requests sent and requests kept for new coverage.
    pub methods: BTreeMap<String, MutationStats>,
    /// Coverage over time.
    pub timeline: Vec<GuideSample>,
    /// Counter-map size each coverage node reported.
    pub counters: BTreeMap<String, u64>,
}

/// The `rpc-coverage.json` report.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuideReport<'a> {
    /// `guided` or `unguided`.
    pub mode: &'static str,
    /// Nodes coverage is read from.
    pub coverage_from: &'a [String],
    /// Seconds since the start.
    pub elapsed_secs: f64,
    /// Requests sent with coverage.
    pub requests: u64,
    /// Requests per second over the run, chain rounds included.
    pub requests_per_sec: f64,
    /// Distinct features (node edge and hit-count bucket).
    pub features: u64,
    /// Distinct edges per node.
    pub edges: BTreeMap<String, u64>,
    /// Counter-map size per node.
    pub counters: &'a BTreeMap<String, u64>,
    /// Corpus size.
    pub corpus: u64,
    /// Readings that failed.
    pub failed_readings: u64,
    /// Fresh and mutated requests, tried and kept.
    pub origins: &'a BTreeMap<String, MutationStats>,
    /// Per mutation operator, children tried and kept.
    pub mutations: &'a BTreeMap<String, MutationStats>,
    /// Per method, requests sent and kept.
    pub methods: &'a BTreeMap<String, MutationStats>,
    /// Coverage over time.
    pub timeline: &'a [GuideSample],
}

/// Coverage-guided request rounds.
///
/// A node reports the edges hit since its previous reading, so the coverage nodes are switched to
/// [`NodeClient::enable_coverage`]: each serializes its requests and reads coverage after every
/// one, so a reading belongs to one request even while the campaign checks determinism,
/// minimizes, imports blocks, or polls filters. Workers still overlap across nodes.
#[derive(Debug)]
pub struct RpcGuide {
    /// Nodes to read coverage from.
    pub coverage_from: Vec<String>,
    /// Mutate corpus requests (`true`) or only generate fresh ones while still measuring
    /// coverage (`false`, the unguided baseline).
    pub guided: bool,
    /// Percentage of guided requests that are fresh instead of mutants.
    pub explore_percent: u64,
    /// Corpus and counters.
    pub state: Mutex<GuideState>,
    /// When the campaign started.
    pub started: Instant,
}

impl RpcGuide {
    /// A guide reading coverage from the `coverage_from` nodes, which it switches to coverage
    /// mode, keeping its corpus in `<out>/rpc-corpus`. Requests saved there by an earlier run
    /// are sent first.
    pub fn new(
        nodes: &mut [NodeClient],
        coverage_from: Vec<String>,
        guided: bool,
        explore_percent: u64,
        out: &std::path::Path,
    ) -> eyre::Result<Self> {
        for name in &coverage_from {
            let node = nodes.iter_mut().find(|n| &n.name == name);
            node.ok_or_else(|| eyre::eyre!("no node named {name}"))?.enable_coverage();
        }
        let (corpus, saved) = Corpus::open(Some(out.join("rpc-corpus")))?;
        Ok(Self {
            coverage_from,
            guided,
            explore_percent,
            state: Mutex::new(GuideState { corpus, queue: saved.into(), ..Default::default() }),
            started: Instant::now(),
        })
    }

    /// One round of `config.requests_per_round` requests over `config.jobs` workers.
    pub fn round(
        &self,
        follower: &ChainFollower,
        config: &RpcConfig,
        round: u64,
        summary: &Mutex<RpcSummary>,
    ) {
        let per_job = config.requests_per_round.div_ceil(config.jobs as u64);
        let chain = (&follower.index, follower.chain.state(), follower.nodes.as_slice());
        thread::scope(|scope| {
            for job in 0..config.jobs {
                thread::Builder::new()
                    .stack_size(RpcCampaign::WORKER_STACK)
                    .spawn_scoped(scope, move || {
                        self.worker(
                            chain,
                            config,
                            summary,
                            [config.seed, round, job as u64],
                            per_job,
                        )
                    })
                    .expect("spawning a request worker");
            }
        });
        let mut s = self.state.lock().expect("lock");
        let sample = GuideSample {
            secs: self.started.elapsed().as_secs(),
            requests: s.requests,
            features: s.corpus.ids.len() as u64,
            edges: s.corpus.names.len() as u64,
            corpus: s.corpus.entries.len() as u64,
        };
        eprintln!(
            "coverage: requests {} features {} edges {} corpus {}",
            sample.requests, sample.features, sample.edges, sample.corpus
        );
        s.timeline.push(sample);
        drop(s);
        if let Err(error) = self.report(&config.out) {
            eprintln!("writing rpc-coverage.json: {error:#}");
        }
    }

    fn worker(
        &self,
        (index, state, nodes): (&ChainIndex, &WorldState, &[NodeClient]),
        config: &RpcConfig,
        summary: &Mutex<RpcSummary>,
        seed: [u64; 3],
        requests: u64,
    ) {
        let mut rng = Rng::new(&[seed[0], seed[1], seed[2], 0x6c1d_e001]);
        let mut mutator = RpcMutator { rng: Rng::new(&[seed[0], seed[1], seed[2], 0x6c1d_e002]) };
        let supported = summary.lock().expect("lock").supported.clone();
        let mut sent = 0;
        while sent < requests {
            let (case, operators) =
                self.next_case(RpcGen { rng: &mut rng, index, state }, &mut mutator, config);
            let Some(serving) = supported.get(&case.method) else {
                continue;
            };
            let serving: Vec<&String> = serving
                .iter()
                .filter(|name| !config.excludes.iter().any(|e| e.matches(name, &case)))
                .collect();
            if serving.len() < 2 {
                continue;
            }
            sent += 1;
            if let Some(path) = &config.request_log {
                RpcCampaign::log_request(path, seed[2] as usize, &case);
            }
            let serving: Vec<&NodeClient> =
                nodes.iter().filter(|n| serving.contains(&&n.name)).collect();
            let mut coverage = BTreeMap::new();
            let mut failed = 0;
            let replies: Vec<(String, Reply)> = serving
                .iter()
                .map(|node| {
                    let (reply, reading) = node.send_covered(&case.body);
                    if node.is_coverage_node() {
                        match reading {
                            Some(reading) => {
                                let mut s = self.state.lock().expect("lock");
                                s.counters.insert(node.name.clone(), reading.counters);
                                for (edge, count) in reading.hits {
                                    coverage.insert(format!("{}:{edge}", node.name), count);
                                }
                            }
                            None => failed += 1,
                        }
                    }
                    (node.name.clone(), reply)
                })
                .collect();
            RpcCampaign::record(&case, &replies, &serving, index.head().number, summary, config);
            self.account(&case, &operators, &coverage, failed);
        }
    }

    /// A saved corpus request, a fresh request, or a mutant of a corpus request.
    fn next_case(
        &self,
        mut generator: RpcGen<'_>,
        mutator: &mut RpcMutator,
        config: &RpcConfig,
    ) -> (RpcCase, Vec<&'static str>) {
        let mut s = self.state.lock().expect("lock");
        if let Some(case) = s.queue.pop_front() {
            return (case, Vec::new());
        }
        if !self.guided || s.corpus.entries.is_empty() || generator.rng.chance(self.explore_percent)
        {
            drop(s);
            return (generator.case(&config.families), Vec::new());
        }
        let parent = s.corpus.pick(generator.rng).expect("corpus is not empty");
        let parent = s.corpus.entries[parent].case.clone();
        let donor = {
            let same: Vec<&RpcCase> = s
                .corpus
                .entries
                .iter()
                .map(|e| &e.case)
                .filter(|c| c.method == parent.method && c.body != parent.body)
                .collect();
            if same.is_empty() || generator.rng.chance(30) {
                let entries = &s.corpus.entries;
                Some(entries[generator.rng.index(entries.len())].case.clone())
            } else {
                Some(same[generator.rng.index(same.len())].clone())
            }
        };
        drop(s);
        let fresh = match parent.family.as_str() {
            // Malformed requests mutate against a well-formed request of any family.
            "malformed" | "filters" => generator.case(&[]),
            family => generator.family(family),
        };
        mutator.mutate(&parent, donor.as_ref(), &fresh)
    }

    /// Counts a request and keeps it if its coverage is new.
    fn account(
        &self,
        case: &RpcCase,
        operators: &[&'static str],
        coverage: &BTreeMap<String, u64>,
        failed: u64,
    ) {
        let mut s = self.state.lock().expect("lock");
        s.requests += 1;
        s.failed_readings += failed;
        let (features, novel) = s.corpus.observe(coverage);
        let kept = novel > 0;
        if kept && let Err(error) = s.corpus.add(case.clone(), features) {
            eprintln!("saving corpus request: {error:#}");
        }
        let origin = if operators.is_empty() { "fresh" } else { "mutant" };
        let entry = s.origins.entry(origin.to_owned()).or_default();
        entry.tried += 1;
        entry.kept += u64::from(kept);
        let entry = s.methods.entry(case.method.clone()).or_default();
        entry.tried += 1;
        entry.kept += u64::from(kept);
        for operator in operators {
            let entry = s.mutations.entry((*operator).to_owned()).or_default();
            entry.tried += 1;
            entry.kept += u64::from(kept);
        }
    }

    /// Rewrites `<out>/rpc-coverage.json`.
    pub fn report(&self, out: &std::path::Path) -> eyre::Result<()> {
        let s = self.state.lock().expect("lock");
        let elapsed = self.started.elapsed().as_secs_f64();
        let mut edges: BTreeMap<String, u64> = BTreeMap::new();
        for name in &s.corpus.names {
            let node = name.rsplit_once(':').map_or(name.as_str(), |(node, _)| node);
            *edges.entry(node.to_owned()).or_default() += 1;
        }
        let report = GuideReport {
            mode: if self.guided { "guided" } else { "unguided" },
            coverage_from: &self.coverage_from,
            elapsed_secs: elapsed,
            requests: s.requests,
            requests_per_sec: s.requests as f64 / elapsed.max(1e-9),
            features: s.corpus.ids.len() as u64,
            edges,
            counters: &s.counters,
            corpus: s.corpus.entries.len() as u64,
            failed_readings: s.failed_readings,
            origins: &s.origins,
            mutations: &s.mutations,
            methods: &s.methods,
            timeline: &s.timeline,
        };
        let tmp = out.join("rpc-coverage.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&report)?)?;
        fs::rename(tmp, out.join("rpc-coverage.json"))?;
        Ok(())
    }
}
