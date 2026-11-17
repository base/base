//! Coverage-guided differential fuzzing seeded from real Base mainnet blocks.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::Decodable2718;
use base_common_consensus::BaseTxEnvelope;
use serde::{Deserialize, Serialize};

use super::{BlockPool, RealCase, RealItem, Transplant};
use crate::{
    Adapter, AdapterResults, AdapterSpec, COVERAGE_ENV, Campaign, Corpus, FindingSignature,
    GeneratorOptions, Keys, Minimizer, Request, Rng, Verdict,
};

/// A case of the mainnet fuzzer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MainCase {
    /// Real transactions on a real pre-state.
    Real(RealCase),
    /// Real contracts and transactions moved into a synthetic generator state.
    Transplant(Transplant),
}

impl MainCase {
    /// Stable id.
    pub fn id(&self) -> String {
        match self {
            Self::Real(case) => case.id(),
            Self::Transplant(case) => case.id(),
        }
    }

    /// The build-mode request.
    pub fn build(&self, pool: &BlockPool) -> eyre::Result<Request> {
        match self {
            Self::Real(case) => case.build(pool),
            Self::Transplant(case) => case.build(pool),
        }
    }

    /// Number of body items, for minimization.
    pub const fn len(&self) -> usize {
        match self {
            Self::Real(case) => case.items.len(),
            Self::Transplant(case) => case.items.len(),
        }
    }

    /// Whether the case has no body items.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The case without body items `range`.
    pub fn without(&self, range: std::ops::Range<usize>) -> Self {
        let mut case = self.clone();
        match &mut case {
            Self::Real(case) => {
                case.items.drain(range);
            }
            Self::Transplant(case) => {
                case.items.drain(range);
            }
        }
        case
    }

    /// The case with its header edits and call edits removed one at a time, as candidates.
    pub fn simplifications(&self) -> Vec<Self> {
        let mut out = Vec::new();
        if let Self::Real(case) = self {
            for i in 0..case.header.len() {
                let mut c = case.clone();
                c.header.remove(i);
                out.push(Self::Real(c));
            }
            for (i, item) in case.items.iter().enumerate() {
                if let RealItem::Deposit { block, index, edits } = item {
                    for e in 0..edits.len() {
                        let mut c = case.clone();
                        let mut edits = edits.clone();
                        edits.remove(e);
                        c.items[i] = RealItem::Deposit { block: *block, index: *index, edits };
                        out.push(Self::Real(c));
                    }
                }
            }
            if case.donors > 0 {
                let window = case.block + case.donors - 1;
                if case.items.iter().all(|item| item.block() <= window) {
                    let mut c = case.clone();
                    c.donors -= 1;
                    out.push(Self::Real(c));
                }
            }
        }
        out
    }
}

/// Settings of [`MainFuzzer`].
#[derive(Debug, Clone)]
pub struct MainFuzzConfig {
    /// Adapters; the reference is started with coverage on.
    pub adapters: Vec<AdapterSpec>,
    /// Reference client name.
    pub reference: String,
    /// Worker threads.
    pub jobs: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Output directory.
    pub out: PathBuf,
    /// How long to run.
    pub duration: Duration,
    /// Seed.
    pub seed: u64,
    /// Percentage of transplant cases among fresh cases.
    pub transplant_percent: u64,
    /// Candidate requests one minimization may run.
    pub minimize_budget: usize,
    /// Feature names of the main corpus, to report what this campaign adds.
    pub baseline: BTreeSet<String>,
}

/// Counters of a mainnet fuzz campaign.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MainFuzzStats {
    /// Cases run.
    pub cases: u64,
    /// Cases per kind (`real` or `transplant`).
    pub kinds: BTreeMap<String, u64>,
    /// Transactions in run cases.
    pub transactions: u64,
    /// Rejected transactions (reference).
    pub rejected: u64,
    /// Cases every adapter agreed on.
    pub agree: u64,
    /// Cases that could not be built.
    pub build_errors: u64,
    /// Adapter failures caused by a pre-state the witness does not cover.
    pub witness_gaps: u64,
    /// Findings matching already reported differences.
    pub known: u64,
    /// Divergences.
    pub diverge: u64,
    /// Other adapter failures.
    pub adapter_failure: u64,
}

/// One finding signature.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MainFindingGroup {
    /// Signature.
    pub signature: String,
    /// Occurrences.
    pub count: u64,
    /// Recorded finding directories.
    pub recorded: Vec<String>,
    /// Minimized reproducers and their transaction counts.
    pub minimized: Vec<(String, usize)>,
}

#[derive(Debug, Default)]
struct State {
    queue: Vec<MainCase>,
    corpus: Vec<(MainCase, Vec<u32>, u64)>,
    ids: HashMap<String, u32>,
    hits: Vec<u64>,
    names: BTreeSet<String>,
    stats: MainFuzzStats,
    findings: BTreeMap<String, MainFindingGroup>,
    gap_errors: BTreeMap<String, u64>,
}

/// Coverage-guided differential fuzzing whose cases come from real mainnet blocks.
#[derive(Debug)]
pub struct MainFuzzer {
    /// Settings.
    pub config: MainFuzzConfig,
    /// Cached mainnet blocks.
    pub pool: Arc<BlockPool>,
}

impl MainFuzzer {
    /// Signatures recorded per kind.
    pub const RECORD_PER_SIGNATURE: usize = 5;
    /// Signatures minimized per kind.
    pub const MINIMIZE_PER_SIGNATURE: usize = 2;

    fn specs(&self) -> Vec<AdapterSpec> {
        self.config
            .adapters
            .iter()
            .map(|spec| {
                let mut spec = spec.clone();
                if spec.name == self.config.reference {
                    let mut command = vec!["env".to_owned(), format!("{COVERAGE_ENV}=1")];
                    command.append(&mut spec.command);
                    spec.command = command;
                }
                spec
            })
            .collect()
    }

    /// Whether an adapter failure is a witness gap: a mutated case read state that none of
    /// the source blocks read, so its trie nodes are not in the witness.
    pub fn is_witness_gap(verdict: &Verdict) -> bool {
        let Verdict::AdapterFailure { failures } = verdict else { return false };
        // A crash or timeout is never a gap, even when another adapter missed a node.
        failures.values().all(|why| {
            let why = why.to_ascii_lowercase();
            why.starts_with("ok=false")
                && [
                    "witness",
                    "missing",
                    "trie node",
                    "unknown node",
                    "blinded",
                    "ancestor",
                    "hash cannot be found",
                ]
                .iter()
                .any(|needle| why.contains(needle))
        })
    }

    /// Whether a finding matches a known, already reported difference:
    /// - base/base alone rejects a transaction whose `gas * maxFeePerGas` overflows u128;
    /// - the block has a transaction from an account at nonce `u64::MAX`.
    pub fn is_known(
        request: &Request,
        results: &AdapterResults,
        signature: &FindingSignature,
        others: &BTreeSet<String>,
        reference: &str,
    ) -> bool {
        let max_nonce = Keys::get().address(Keys::MAX_NONCE);
        let decoded: Vec<Option<BaseTxEnvelope>> = request
            .transactions
            .iter()
            .map(|raw| BaseTxEnvelope::decode_2718(&mut raw.as_ref()).ok())
            .collect();
        let sender = |tx: &BaseTxEnvelope| match tx {
            BaseTxEnvelope::Deposit(d) => Some(d.from),
            other => other.recover_signer().ok(),
        };
        if decoded.iter().flatten().any(|tx| sender(tx) == Some(max_nonce)) {
            return true;
        }
        if signature.failure || signature.odd != *others {
            return false;
        }
        let rejected = |name: &str| -> BTreeSet<usize> {
            results
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, r)| r.as_ref().ok())
                .map(|r| r.rejected.iter().map(|x| x.index).collect())
                .unwrap_or_default()
        };
        let by_reference = rejected(reference);
        let by_other = others.iter().next().map(|o| rejected(o)).unwrap_or_default();
        let overflow: Vec<usize> = by_reference.difference(&by_other).copied().collect();
        !overflow.is_empty()
            && overflow.iter().all(|i| {
                decoded.get(*i).and_then(Option::as_ref).is_some_and(|tx| {
                    (tx.gas_limit() as u128).checked_mul(tx.max_fee_per_gas()).is_none()
                })
            })
    }

    /// Runs the campaign.
    pub fn run(&self) -> eyre::Result<MainFuzzStats> {
        fs::create_dir_all(self.config.out.join("findings"))?;
        fs::create_dir_all(self.config.out.join("corpus"))?;
        // Cases saved by an earlier run with the same output directory run first, so a
        // restarted campaign (for example to pick up newly fetched blocks) keeps its corpus.
        let mut queue = Vec::new();
        for entry in fs::read_dir(self.config.out.join("corpus"))? {
            let path = entry?.path();
            if let Ok(case) = serde_json::from_slice::<MainCase>(&fs::read(&path)?) {
                queue.push(case);
            }
        }
        eprintln!("{} saved corpus cases to rerun", queue.len());
        let state = Mutex::new(State { queue, ..Default::default() });
        let started = Instant::now();
        let deadline = started + self.config.duration;
        let specs = self.specs();
        thread::scope(|scope| {
            for worker in 0..self.config.jobs.max(1) {
                let (state, specs) = (&state, &specs);
                scope.spawn(move || {
                    let mut adapters: Vec<Adapter> = specs
                        .iter()
                        .map(|s| Adapter::new(s.clone(), self.config.timeout))
                        .collect();
                    let mut rng = Rng::new(&[self.config.seed, worker as u64, 0x6d61_696e]);
                    let mut last_report = Instant::now();
                    while Instant::now() < deadline {
                        self.round(state, &mut adapters, &mut rng);
                        if worker == 0 && last_report.elapsed() > Duration::from_secs(20) {
                            last_report = Instant::now();
                            if let Err(error) = self.report(&state.lock().unwrap(), started) {
                                eprintln!("report: {error:#}");
                            }
                        }
                    }
                });
            }
        });
        let s = state.into_inner().unwrap();
        self.report(&s, started)?;
        Ok(s.stats)
    }

    fn next_case(&self, state: &Mutex<State>, rng: &mut Rng) -> Option<MainCase> {
        let (parent, donor) = {
            let mut s = state.lock().unwrap();
            if let Some(case) = s.queue.pop() {
                return Some(case);
            }
            if s.corpus.is_empty() || rng.chance(20) {
                (None, None)
            } else {
                // Half of the mutations go to each kind, so neither crowds out the other.
                let real = rng.chance(50);
                let weights: Vec<u64> = s
                    .corpus
                    .iter()
                    .map(|(case, features, picks)| {
                        if matches!(case, MainCase::Real(_)) != real {
                            return 0;
                        }
                        let rarity: f64 =
                            features.iter().map(|id| 1.0 / s.hits[*id as usize] as f64).sum();
                        (1_000.0 * rarity / (1.0 + *picks as f64 / 8.0).sqrt()) as u64 + 1
                    })
                    .collect();
                if weights.iter().all(|w| *w == 0) {
                    return self.fresh_case(rng);
                }
                let i = rng.weighted(&weights);
                s.corpus[i].2 += 1;
                let donor = s.corpus[rng.index(s.corpus.len())].0.clone();
                (Some(s.corpus[i].0.clone()), Some(donor))
            }
        };
        match parent {
            Some(MainCase::Real(case)) => {
                let donor = match &donor {
                    Some(MainCase::Real(d)) => Some(d),
                    _ => None,
                };
                Some(MainCase::Real(case.mutate(&self.pool, rng, donor)))
            }
            Some(MainCase::Transplant(case)) => {
                Some(MainCase::Transplant(case.mutate(&self.pool, rng)))
            }
            None => self.fresh_case(rng),
        }
    }

    fn fresh_case(&self, rng: &mut Rng) -> Option<MainCase> {
        if rng.chance(self.config.transplant_percent) {
            return Transplant::fresh(&self.pool, rng).map(MainCase::Transplant);
        }
        let donors = rng.below(3);
        let block = self.pool.pick_block(rng, donors)?;
        let mut case = RealCase::identity(&self.pool, block).ok()?;
        case.donors = donors;
        if rng.chance(70) {
            case = case.mutate(&self.pool, rng, None);
        }
        Some(MainCase::Real(case))
    }

    fn round(&self, state: &Mutex<State>, adapters: &mut [Adapter], rng: &mut Rng) {
        let Some(case) = self.next_case(state, rng) else {
            state.lock().unwrap().stats.build_errors += 1;
            return;
        };
        let request = match case.build(&self.pool) {
            Ok(request) => request,
            Err(_) => {
                state.lock().unwrap().stats.build_errors += 1;
                return;
            }
        };
        let (results, verdict) = Minimizer::run(adapters, &request);
        let coverage = results
            .iter()
            .find(|(name, _)| *name == self.config.reference)
            .and_then(|(_, r)| r.as_ref().ok())
            .and_then(|r| r.coverage.clone());
        let mut s = state.lock().unwrap();
        s.stats.cases += 1;
        let kind = match case {
            MainCase::Real(_) => "real",
            MainCase::Transplant(_) => "transplant",
        };
        *s.stats.kinds.entry(kind.to_owned()).or_default() += 1;
        s.stats.transactions += request.transactions.len() as u64;
        if let Some(coverage) = coverage {
            let mut features = Vec::new();
            let mut novel = 0;
            for (name, count) in coverage.iter().filter(|(_, c)| **c > 0) {
                let key = format!("{name}#{}", <Corpus>::bucket(*count));
                let next = s.ids.len() as u32;
                let id = *s.ids.entry(key).or_insert(next);
                if id == next {
                    s.hits.push(0);
                    novel += 1;
                    s.names.insert(name.clone());
                }
                s.hits[id as usize] += 1;
                features.push(id);
            }
            if novel > 0 {
                let path = self.config.out.join("corpus").join(format!("{}.json", case.id()));
                let _ = fs::write(path, serde_json::to_vec(&case).unwrap_or_default());
                s.corpus.push((case.clone(), features, 0));
            }
        }
        if let Some((_, Ok(response))) =
            results.iter().find(|(name, _)| *name == self.config.reference)
        {
            s.stats.rejected += response.rejected.len() as u64;
        }
        if verdict == Verdict::Agree {
            s.stats.agree += 1;
            return;
        }
        if Self::is_witness_gap(&verdict) {
            s.stats.witness_gaps += 1;
            if let Verdict::AdapterFailure { failures } = &verdict {
                for (name, why) in failures {
                    let short: String = why.chars().take(90).collect();
                    *s.gap_errors.entry(format!("{name}: {short}")).or_default() += 1;
                }
            }
            return;
        }
        let Some(signature) = FindingSignature::of(&verdict, &self.config.reference) else {
            return;
        };
        let others: BTreeSet<String> = self
            .config
            .adapters
            .iter()
            .map(|a| a.name.clone())
            .filter(|n| *n != self.config.reference)
            .collect();
        if Self::is_known(&request, &results, &signature, &others, &self.config.reference) {
            s.stats.known += 1;
            return;
        }
        if signature.failure {
            s.stats.adapter_failure += 1;
        } else {
            s.stats.diverge += 1;
        }
        let key = signature.describe();
        let group = s.findings.entry(key.clone()).or_insert_with(|| MainFindingGroup {
            signature: key.clone(),
            count: 0,
            recorded: Vec::new(),
            minimized: Vec::new(),
        });
        group.count += 1;
        let record = group.recorded.len() < Self::RECORD_PER_SIGNATURE;
        let minimize = group.recorded.len() < Self::MINIMIZE_PER_SIGNATURE;
        if !record {
            return;
        }
        let dir = self.config.out.join("findings").join(case.id());
        group.recorded.push(dir.display().to_string());
        drop(s);
        eprintln!("[{}] {key}: {}", case.id(), verdict.describe());
        let line = serde_json::to_string(&request).expect("requests serialize");
        let stripped = Self::strip(&results);
        if let Err(error) = self.campaign().record(&request, &line, &stripped, &verdict) {
            eprintln!("recording {}: {error:#}", case.id());
        }
        let _ =
            fs::write(dir.join("case.json"), serde_json::to_vec_pretty(&case).unwrap_or_default());
        if !minimize || self.config.minimize_budget == 0 {
            return;
        }
        let (min_case, min_request, attempts) = self.minimize(adapters, &case, &signature);
        let (min_results, min_verdict) = Minimizer::run(adapters, &min_request);
        let _ = fs::write(
            dir.join("minimized-case.json"),
            serde_json::to_vec_pretty(&min_case).unwrap_or_default(),
        );
        let _ = crate::Fuzzer::write_minimized(
            &dir,
            &min_request,
            &min_verdict,
            &Self::strip(&min_results),
        );
        eprintln!(
            "[{}] minimized to {} transactions in {attempts} runs ({})",
            case.id(),
            min_request.transactions.len(),
            min_verdict.describe()
        );
        let mut s = state.lock().unwrap();
        if let Some(group) = s.findings.get_mut(&key) {
            group.minimized.push((
                dir.join("minimized.json").display().to_string(),
                min_request.transactions.len(),
            ));
        }
    }

    fn strip(results: &AdapterResults) -> AdapterResults {
        results
            .iter()
            .map(|(n, r)| {
                (n.clone(), r.as_ref().map(crate::Fuzzer::without_coverage).map_err(Clone::clone))
            })
            .collect()
    }

    /// Reduces a finding: chunks of body items, then edits, then single transactions of the
    /// request.
    fn minimize(
        &self,
        adapters: &mut [Adapter],
        case: &MainCase,
        target: &FindingSignature,
    ) -> (MainCase, Request, usize) {
        let mut attempts = 0;
        let budget = self.config.minimize_budget;
        let reference = self.config.reference.clone();
        let mut test = |candidate: &MainCase, attempts: &mut usize| -> bool {
            if *attempts >= budget {
                return false;
            }
            *attempts += 1;
            let Ok(request) = candidate.build(&self.pool) else { return false };
            let (_, verdict) = Minimizer::run(adapters, &request);
            FindingSignature::of(&verdict, &reference).is_some_and(|s| target.reproduced_by(&s))
        };
        let mut best = case.clone();
        let mut chunk = (best.len() / 2).max(1);
        loop {
            let mut i = 0;
            while i < best.len() && best.len() > 1 {
                let end = (i + chunk).min(best.len());
                let candidate = best.without(i..end);
                if !candidate.is_empty() && test(&candidate, &mut attempts) {
                    best = candidate;
                } else {
                    i += chunk;
                }
            }
            if chunk == 1 {
                break;
            }
            chunk /= 2;
        }
        let mut progress = true;
        while progress {
            progress = false;
            for candidate in best.simplifications() {
                if test(&candidate, &mut attempts) {
                    best = candidate;
                    progress = true;
                    break;
                }
            }
        }
        let request = best.build(&self.pool).expect("the minimized case built before");
        let options = GeneratorOptions::default();
        let mut minimizer = Minimizer {
            adapters,
            reference: &self.config.reference,
            options: &options,
            budget: budget.saturating_sub(attempts).max(20),
            attempts: 0,
        };
        let minimized = minimizer.minimize_request(&request, target);
        (best, minimized.request, attempts + minimized.attempts)
    }

    fn campaign(&self) -> Campaign {
        Campaign {
            adapters: self.config.adapters.clone(),
            jobs: self.config.jobs,
            timeout: self.config.timeout,
            out: self.config.out.clone(),
            max_findings: u64::MAX,
        }
    }

    fn report(&self, s: &State, started: Instant) -> eyre::Result<()> {
        let elapsed = started.elapsed().as_secs_f64();
        let family = |name: &str| name.split(':').next().unwrap_or("").to_owned();
        let mut families: BTreeMap<String, u64> = BTreeMap::new();
        for name in &s.names {
            *families.entry(family(name)).or_default() += 1;
        }
        let beyond: Vec<&String> =
            s.names.iter().filter(|name| !self.config.baseline.contains(*name)).collect();
        let mut beyond_families: BTreeMap<String, u64> = BTreeMap::new();
        for name in &beyond {
            *beyond_families.entry(family(name)).or_default() += 1;
        }
        let report = serde_json::json!({
            "elapsedSecs": elapsed,
            "casesPerSec": s.stats.cases as f64 / elapsed.max(1e-9),
            "stats": s.stats,
            "features": s.ids.len(),
            "names": s.names.len(),
            "families": families,
            "corpus": s.corpus.len(),
            "baselineNames": self.config.baseline.len(),
            "namesBeyondBaseline": beyond.len(),
            "beyondBaselineFamilies": beyond_families,
            "findings": s.findings.values().collect::<Vec<_>>(),
            "witnessGapErrors": s.gap_errors.iter().rev().take(20).collect::<BTreeMap<_, _>>(),
        });
        eprintln!(
            "[{:.0}s] cases {} agree {} gaps {} known {} diverge {} failure {} names {} (+{} beyond baseline) corpus {}",
            elapsed,
            s.stats.cases,
            s.stats.agree,
            s.stats.witness_gaps,
            s.stats.known,
            s.stats.diverge,
            s.stats.adapter_failure,
            s.names.len(),
            beyond.len(),
            s.corpus.len()
        );
        let tmp = self.config.out.join("coverage.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&report)?)?;
        fs::rename(&tmp, self.config.out.join("coverage.json"))?;
        let names: Vec<&str> = s.names.iter().map(String::as_str).collect();
        fs::write(self.config.out.join("names.txt"), names.join("\n"))?;
        let beyond: Vec<&str> = beyond.iter().map(|s| s.as_str()).collect();
        fs::write(self.config.out.join("beyond-baseline.txt"), beyond.join("\n"))?;
        Ok(())
    }
}
