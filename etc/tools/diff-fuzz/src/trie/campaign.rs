//! Coverage-guided differential trie fuzzing: cases whose run reaches an edge no earlier case
//! reached in any coverage adapter, again on a second run, are kept and mutated; every case is compared across all
//! implementations, and divergences are recorded and minimized.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    Adapter, AdapterSpec, COVERAGE_ENV, Corpus, Rng, TrieBlockResult, TrieCase, TrieCompare,
    TrieGen, TrieMutator, TrieResponse, TrieVerdict,
};

/// Settings of a trie fuzzing run.
#[derive(Debug, Clone)]
pub struct TrieFuzzConfig {
    /// Every adapter; the first is the reference whose feature names have no client prefix.
    pub adapters: Vec<AdapterSpec>,
    /// Adapters started with coverage on.
    pub coverage_from: Vec<String>,
    /// Worker threads, each with its own process of every adapter.
    pub jobs: usize,
    /// How long to run.
    pub duration: Duration,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Output directory.
    pub out: PathBuf,
    /// Corpus directory (guided runs only).
    pub corpus: Option<PathBuf>,
    /// Scratch directory for adapters' databases.
    pub scratch: PathBuf,
    /// Base seed.
    pub seed: u64,
    /// Percentage of guided rounds that run a fresh case.
    pub explore_percent: u64,
    /// Run only fresh cases (still measuring coverage).
    pub unguided: bool,
    /// Interval of timeline samples.
    pub sample_every: Duration,
    /// Candidate runs one minimization may use (0 disables it).
    pub minimize_budget: usize,
    /// Signature substrings of known causes, counted but not recorded (see
    /// [`TrieFuzzer::KNOWN`]).
    pub known: Vec<String>,
}

/// One timeline sample.
#[derive(Debug, Clone, Serialize)]
pub struct TrieSample {
    /// Seconds since the start.
    pub secs: u64,
    /// Cases run.
    pub execs: u64,
    /// Kept cases.
    pub corpus: usize,
    /// Distinct edges per client.
    pub edges: BTreeMap<String, usize>,
    /// Distinct finding signatures.
    pub signatures: usize,
}

/// A recorded group of findings with one signature.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TrieFinding {
    /// Times it occurred.
    pub count: u64,
    /// Occurrences whose case keys share at most
    /// [`TrieGen::REALISTIC_DEPTH`](crate::TrieGen::REALISTIC_DEPTH) leading nibbles, which keccak
    /// keys of a real trie can.
    pub realistic: u64,
    /// Occurrences that need deeper shared prefixes, unreachable for keccak keys.
    pub deep: u64,
    /// The shallowest case depth (longest shared key prefix) it occurred at, including
    /// minimized reproducers.
    pub min_depth: Option<usize>,
    /// Recorded finding directories.
    pub recorded: Vec<String>,
    /// Minimized reproducers.
    pub minimized: Vec<String>,
}

/// Counters and results of a run, written to `trie-coverage.json`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TrieFuzzStats {
    /// Whether the run was guided.
    pub guided: bool,
    /// Cases run.
    pub execs: u64,
    /// Cases kept for new edges.
    pub kept: u64,
    /// Second runs of cases that reached new edges (see `stable_novelty`).
    pub reruns: u64,
    /// Cases no coverage adapter returned coverage for.
    pub no_coverage: u64,
    /// Distinct edges per client.
    pub edges: BTreeMap<String, usize>,
    /// Findings by signature.
    pub findings: BTreeMap<String, TrieFinding>,
    /// Occurrences of known causes, by the known signature substring.
    pub known: BTreeMap<String, u64>,
    /// Per mutation: uses and kept cases.
    pub mutations: BTreeMap<String, (u64, u64)>,
    /// Timeline.
    pub timeline: Vec<TrieSample>,
}

struct Shared {
    corpus: Corpus<TrieCase>,
    stats: TrieFuzzStats,
    edges: BTreeMap<String, HashSet<String>>,
}

/// Every implementation's results for one case: the model first, then each adapter's primary
/// and its variants as `adapter/variant`.
pub type TrieResults = Vec<(String, Result<Vec<TrieBlockResult>, String>)>;

/// The trie fuzzer.
#[derive(Debug)]
pub struct TrieFuzzer {
    /// Settings.
    pub config: TrieFuzzConfig,
}

impl TrieFuzzer {
    /// Known causes, matched as signature substrings:
    ///
    /// - alloy-trie's hash builder with updates panics on a branch whose RLP is short enough to
    ///   embed in its parent (two storage slots sharing about 58 nibbles or more), which every
    ///   reth path that keeps trie updates hits.
    pub const KNOWN: [&str; 1] = ["panicked: cannot convert a slice of length # to FixedBytes<#"];

    /// The client a feature name belongs to: its prefix before `:edge:`, or the reference.
    fn client<'a>(&'a self, feature: &'a str) -> &'a str {
        match feature.split_once(":edge:") {
            Some((client, _)) => client,
            None => &self.config.adapters[0].name,
        }
    }

    /// Starts one process of every adapter, coverage on where asked.
    pub fn adapters(&self) -> Vec<Adapter> {
        self.config
            .adapters
            .iter()
            .map(|spec| {
                let mut spec = spec.clone();
                if self.config.coverage_from.contains(&spec.name) {
                    let mut command = vec!["env".to_owned(), format!("{COVERAGE_ENV}=1")];
                    command.extend(spec.command);
                    spec.command = command;
                }
                Adapter::new(spec, self.config.timeout)
            })
            .collect()
    }

    /// Cases a worker runs before it restarts its adapters. Every request opens fresh
    /// databases, and resources a database leaves behind when closed (such as the read
    /// transaction monitor thread of each MDBX environment) would otherwise pile up.
    pub const RESTART_EVERY: u64 = 250;

    /// Runs `case` on every adapter (in parallel) and returns every implementation's results
    /// and the merged coverage of the coverage adapters.
    pub fn run_case(
        &self,
        adapters: &mut [Adapter],
        case: &TrieCase,
        id: &str,
        scratch: &Path,
    ) -> (TrieResults, Option<BTreeMap<String, u64>>) {
        let built = case.build(id.to_owned(), String::new());
        let responses: Vec<Result<TrieResponse, String>> = thread::scope(|scope| {
            let handles: Vec<_> = adapters
                .iter_mut()
                .map(|adapter| {
                    let mut request = built.request.clone();
                    let dir = scratch.join(&adapter.spec.name);
                    let _ = fs::remove_dir_all(&dir);
                    request.trie.dir = dir.display().to_string();
                    scope.spawn(move || {
                        let line = serde_json::to_string(&request).expect("requests serialize");
                        let result = adapter.call_as::<TrieResponse>(&request.id, &line);
                        let _ = fs::remove_dir_all(&dir);
                        result.map_err(|err| err.to_string())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("adapter threads do not panic")).collect()
        });
        let mut results: TrieResults = vec![("model".to_owned(), Ok(built.expected))];
        let mut coverage: Option<BTreeMap<String, u64>> = None;
        for (adapter, response) in adapters.iter().zip(responses) {
            let name = adapter.spec.name.clone();
            match response {
                Err(err) => results.push((name, Err(err))),
                Ok(response) if !response.ok => {
                    results.push((name, Err(response.error.unwrap_or_default())));
                }
                Ok(response) => {
                    if self.config.coverage_from.contains(&name)
                        && let Some(features) = response.coverage
                    {
                        coverage.get_or_insert_default().extend(features);
                    }
                    let blocks = case.blocks.len();
                    if response.blocks.len() != blocks {
                        let got = response.blocks.len();
                        results
                            .push((name.clone(), Err(format!("{got} block results for {blocks}"))));
                    } else {
                        results.push((name.clone(), Ok(response.blocks)));
                    }
                    for (variant, list) in response.variants {
                        results.push((format!("{name}/{variant}"), Ok(list)));
                    }
                }
            }
        }
        (results, coverage)
    }

    /// Runs until the duration elapses and returns the counters.
    pub fn run(&self) -> eyre::Result<TrieFuzzStats> {
        fs::create_dir_all(self.config.out.join("findings"))?;
        let corpus_dir = if self.config.unguided { None } else { self.config.corpus.clone() };
        let (corpus, saved) = Corpus::<TrieCase>::open(corpus_dir)?;
        let shared = Mutex::new(Shared {
            corpus,
            stats: TrieFuzzStats { guided: !self.config.unguided, ..Default::default() },
            edges: BTreeMap::new(),
        });
        let saved = Mutex::new(saved);
        let start = Instant::now();
        let deadline = start + self.config.duration;
        thread::scope(|scope| {
            for job in 0..self.config.jobs {
                let (shared, saved) = (&shared, &saved);
                scope.spawn(move || self.worker(job, shared, saved, start, deadline));
            }
            let mut next_sample = start;
            let mut next_report = start;
            while Instant::now() < deadline {
                let now = Instant::now();
                if now >= next_sample {
                    self.sample(&shared, start);
                    next_sample += self.config.sample_every;
                }
                if now >= next_report {
                    if let Err(err) = self.report(&shared) {
                        eprintln!("writing trie coverage report: {err:#}");
                    }
                    next_report += Duration::from_secs(10);
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        self.sample(&shared, start);
        self.report(&shared)?;
        Ok(shared.into_inner().expect("workers ended").stats)
    }

    fn sample(&self, shared: &Mutex<Shared>, start: Instant) {
        let mut s = shared.lock().expect("no worker panics holding the lock");
        let sample = TrieSample {
            secs: start.elapsed().as_secs(),
            execs: s.stats.execs,
            corpus: s.corpus.entries.len(),
            edges: s.edges.iter().map(|(c, e)| (c.clone(), e.len())).collect(),
            signatures: s.stats.findings.len(),
        };
        s.stats.timeline.push(sample);
    }

    fn report(&self, shared: &Mutex<Shared>) -> eyre::Result<()> {
        let mut s = shared.lock().expect("no worker panics holding the lock");
        let edges: BTreeMap<String, usize> =
            s.edges.iter().map(|(c, e)| (c.clone(), e.len())).collect();
        s.stats.edges = edges;
        let text = serde_json::to_vec_pretty(&s.stats)?;
        drop(s);
        let path = self.config.out.join("trie-coverage.json");
        let tmp = self.config.out.join("trie-coverage.json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn worker(
        &self,
        job: usize,
        shared: &Mutex<Shared>,
        saved: &Mutex<Vec<TrieCase>>,
        start: Instant,
        deadline: Instant,
    ) {
        let mut adapters = self.adapters();
        let scratch = self.config.scratch.join(format!("{}-job{job}", std::process::id()));
        let mut rng = Rng::new(&[self.config.seed, job as u64, start.elapsed().as_nanos() as u64]);
        let mut round = 0u64;
        while Instant::now() < deadline {
            round += 1;
            if round.is_multiple_of(Self::RESTART_EVERY) {
                adapters.iter_mut().for_each(Adapter::restart);
            }
            let (case, mutations) = self.next_case(&mut rng, shared, saved, job, round);
            let id = format!("trie-{job}-{round}");
            let (results, coverage) = self.run_case(&mut adapters, &case, &id, &scratch);
            let verdict = TrieCompare::compare(case.blocks.len(), &results);
            let mut coverage = coverage.map(|mut coverage| {
                coverage.values_mut().for_each(|count| *count = 1);
                coverage
            });
            let stable =
                self.stable_novelty(&mut adapters, shared, &case, &id, &scratch, &coverage);
            let mut record = None;
            {
                let mut s = shared.lock().expect("no worker panics holding the lock");
                s.stats.execs += 1;
                let novel = match coverage.take() {
                    Some(coverage) => {
                        for name in coverage.keys() {
                            let client = self.client(name).to_owned();
                            s.edges.entry(client).or_default().insert(name.clone());
                        }
                        let (features, novel) = s.corpus.observe(&coverage);
                        let novel = if stable { novel } else { 0 };
                        if novel > 0 && !self.config.unguided {
                            if let Err(err) = s.corpus.add(case.clone(), features) {
                                eprintln!("saving corpus case: {err:#}");
                            }
                            s.stats.kept += 1;
                        }
                        novel > 0
                    }
                    None => {
                        s.stats.no_coverage += 1;
                        false
                    }
                };
                for name in &mutations {
                    let entry = s.stats.mutations.entry((*name).to_owned()).or_default();
                    entry.0 += 1;
                    entry.1 += u64::from(novel);
                }
                let signature = verdict.signature();
                let known = signature.as_ref().and_then(|signature| {
                    self.config.known.iter().find(|known| signature.contains(known.as_str()))
                });
                if let Some(known) = known {
                    *s.stats.known.entry(known.clone()).or_default() += 1;
                } else if let Some(signature) = signature {
                    let depth = case.depth();
                    let finding = s.stats.findings.entry(signature.clone()).or_default();
                    finding.count += 1;
                    if depth <= TrieGen::REALISTIC_DEPTH {
                        finding.realistic += 1;
                    } else {
                        finding.deep += 1;
                    }
                    finding.min_depth = Some(finding.min_depth.map_or(depth, |d| d.min(depth)));
                    if finding.recorded.len() < 5 {
                        let dir = format!("{}-{}", Self::slug(&signature), finding.count);
                        finding.recorded.push(dir.clone());
                        let minimize =
                            finding.recorded.len() <= 2 && self.config.minimize_budget > 0;
                        record = Some((signature, dir, minimize));
                    }
                }
            }
            if let Some((signature, dir, minimize)) = record {
                let path = self.config.out.join("findings").join(&dir);
                if let Err(err) = Self::save(&path, "", &case, &results, &verdict) {
                    eprintln!("saving finding: {err:#}");
                }
                if minimize {
                    let small = self.minimize(&mut adapters, &case, &signature, &scratch);
                    let id = format!("{id}-min");
                    let (results, _) = self.run_case(&mut adapters, &small, &id, &scratch);
                    let verdict = TrieCompare::compare(small.blocks.len(), &results);
                    if let Err(err) = Self::save(&path, "minimized-", &small, &results, &verdict) {
                        eprintln!("saving minimized finding: {err:#}");
                    }
                    let mut s = shared.lock().expect("no worker panics holding the lock");
                    if let Some(finding) = s.stats.findings.get_mut(&signature) {
                        finding.minimized.push(dir);
                        let depth = small.depth();
                        finding.min_depth = Some(finding.min_depth.map_or(depth, |d| d.min(depth)));
                    }
                }
            }
        }
    }

    /// Whether a guided case reached an edge no earlier case reached that a second run reaches
    /// again. Some edges differ between runs of the same request (Nethermind's previous-location
    /// state is per thread, so the first edge of work on a pool thread depends on what that
    /// thread ran before), and keeping cases for those would fill the corpus with noise.
    fn stable_novelty(
        &self,
        adapters: &mut [Adapter],
        shared: &Mutex<Shared>,
        case: &TrieCase,
        id: &str,
        scratch: &Path,
        coverage: &Option<BTreeMap<String, u64>>,
    ) -> bool {
        let Some(coverage) = coverage.as_ref().filter(|_| !self.config.unguided) else {
            return false;
        };
        let novel: Vec<&String> = {
            let s = shared.lock().expect("no worker panics holding the lock");
            coverage.keys().filter(|name| !s.corpus.names.contains(*name)).collect()
        };
        if novel.is_empty() {
            return false;
        }
        shared.lock().expect("no worker panics holding the lock").stats.reruns += 1;
        let (_, again) = self.run_case(adapters, case, &format!("{id}-again"), scratch);
        let again = again.unwrap_or_default();
        novel.into_iter().any(|name| again.contains_key(name))
    }

    fn next_case(
        &self,
        rng: &mut Rng,
        shared: &Mutex<Shared>,
        saved: &Mutex<Vec<TrieCase>>,
        job: usize,
        round: u64,
    ) -> (TrieCase, Vec<&'static str>) {
        if let Some(case) = saved.lock().expect("no panics holding the lock").pop() {
            return (case, Vec::new());
        }
        let fresh = || TrieGen::new(Rng::new(&[self.config.seed, job as u64, round])).case();
        if self.config.unguided || rng.chance(self.config.explore_percent) {
            return (fresh(), Vec::new());
        }
        let (parent, donor) = {
            let mut s = shared.lock().expect("no worker panics holding the lock");
            let Some(index) = s.corpus.pick(rng) else { return (fresh(), Vec::new()) };
            let parent = s.corpus.entries[index].case.clone();
            let donor = s.corpus.entries[rng.index(s.corpus.entries.len())].case.clone();
            (parent, donor)
        };
        let mut case = parent;
        let mutations = TrieMutator::mutate(rng, &mut case, Some(&donor));
        (case, mutations)
    }

    fn slug(signature: &str) -> String {
        let slug: String = signature
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        slug.chars().take(80).collect()
    }

    fn save(
        dir: &Path,
        prefix: &str,
        case: &TrieCase,
        results: &TrieResults,
        verdict: &TrieVerdict,
    ) -> eyre::Result<()> {
        fs::create_dir_all(dir)?;
        let request = case.build(format!("{prefix}repro"), "/tmp/diff-trie-repro".to_owned());
        fs::write(dir.join(format!("{prefix}case.json")), serde_json::to_vec_pretty(case)?)?;
        fs::write(
            dir.join(format!("{prefix}request.json")),
            serde_json::to_vec(&request.request)?,
        )?;
        let results: BTreeMap<&String, &Result<Vec<TrieBlockResult>, String>> =
            results.iter().map(|(n, r)| (n, r)).collect();
        fs::write(dir.join(format!("{prefix}results.json")), serde_json::to_vec_pretty(&results)?)?;
        fs::write(dir.join(format!("{prefix}verdict.json")), serde_json::to_vec_pretty(verdict)?)?;
        Ok(())
    }

    /// Shrinks `case` while it still produces `signature`: blocks after the first divergence,
    /// whole blocks, runs of ops, proof reads and slots, and unwinds.
    pub fn minimize(
        &self,
        adapters: &mut [Adapter],
        case: &TrieCase,
        signature: &str,
        scratch: &Path,
    ) -> TrieCase {
        let budget = std::cell::Cell::new(self.config.minimize_budget);
        let mut best = case.clone();
        let mut attempt = 0u64;
        let mut reproduces = |candidate: &TrieCase, adapters: &mut [Adapter]| -> bool {
            if budget.get() == 0 || candidate.blocks.is_empty() {
                return false;
            }
            budget.set(budget.get() - 1);
            attempt += 1;
            let id = format!("min-{attempt}");
            let (results, _) = self.run_case(adapters, candidate, &id, scratch);
            TrieCompare::compare(candidate.blocks.len(), &results).signature().as_deref()
                == Some(signature)
        };
        // Blocks after the first diverging one cannot matter.
        let (results, _) = self.run_case(adapters, &best, "min-0", scratch);
        if let Some(first) = TrieCompare::compare(best.blocks.len(), &results).differences.first() {
            let mut cut = best.clone();
            cut.blocks.truncate(first.block + 1);
            if cut.blocks.len() < best.blocks.len() && reproduces(&cut, adapters) {
                best = cut;
            }
        }
        let mut progress = true;
        while progress {
            progress = false;
            for i in (0..best.blocks.len()).rev() {
                let mut candidate = best.clone();
                candidate.blocks.remove(i);
                if reproduces(&candidate, adapters) {
                    best = candidate;
                    progress = true;
                }
            }
            for i in 0..best.blocks.len() {
                let mut chunk = best.blocks[i].ops.len().div_ceil(2).max(1);
                while chunk > 0 {
                    let mut start = 0;
                    while start < best.blocks[i].ops.len() {
                        let mut candidate = best.clone();
                        let ops = &mut candidate.blocks[i].ops;
                        let end = (start + chunk).min(ops.len());
                        ops.drain(start..end);
                        if reproduces(&candidate, adapters) {
                            best = candidate;
                            progress = true;
                        } else {
                            start += chunk;
                        }
                    }
                    chunk /= 2;
                }
                for p in (0..best.blocks[i].proofs.len()).rev() {
                    let mut candidate = best.clone();
                    candidate.blocks[i].proofs.remove(p);
                    if reproduces(&candidate, adapters) {
                        best = candidate;
                        progress = true;
                        continue;
                    }
                    for s in (0..best.blocks[i].proofs[p].slots.len()).rev() {
                        let mut candidate = best.clone();
                        candidate.blocks[i].proofs[p].slots.remove(s);
                        if reproduces(&candidate, adapters) {
                            best = candidate;
                            progress = true;
                        }
                    }
                }
                if best.blocks[i].unwind > 0 {
                    let mut candidate = best.clone();
                    candidate.blocks[i].unwind = 0;
                    if reproduces(&candidate, adapters) {
                        best = candidate;
                        progress = true;
                    }
                }
            }
            if budget.get() == 0 {
                break;
            }
        }
        best
    }
}
