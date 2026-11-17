//! The coverage-guided fuzz loop.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    Adapter, AdapterSpec, COVERAGE_ENV, CallFailure, Campaign, Corpus, FindingSignature, FuzzCase,
    GeneratorOptions, Minimizer, Mutation, Mutator, Request, Response, Rng, Template, Verdict,
};

/// Settings of a fuzz run.
#[derive(Debug, Clone)]
pub struct FuzzConfig {
    /// Adapters to compare.
    pub adapters: Vec<AdapterSpec>,
    /// Name of the reference adapter, which reports coverage.
    pub reference: String,
    /// Worker threads; each runs one process of every adapter.
    pub jobs: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Output directory for findings and `coverage.json`.
    pub out: PathBuf,
    /// Corpus directory (`None` keeps the corpus in memory).
    pub corpus: Option<PathBuf>,
    /// How long to run.
    pub duration: Duration,
    /// Generator seed.
    pub seed: u64,
    /// Generator options.
    pub options: GeneratorOptions,
    /// Mutate corpus cases (`true`) or only generate fresh cases (`false`, the unguided
    /// baseline, which still measures coverage).
    pub guided: bool,
    /// Fresh cases to start from when the corpus is empty.
    pub initial: u64,
    /// Percentage of guided rounds that try a fresh case instead of a mutation.
    pub explore_percent: u64,
    /// Findings recorded per signature.
    pub record_per_signature: u64,
    /// Findings minimized per signature.
    pub minimize_per_signature: u64,
    /// Candidate requests one minimization may run.
    pub minimize_budget: usize,
    /// Stop after this many distinct finding signatures.
    pub max_findings: u64,
    /// How often to add a sample to the coverage timeline.
    pub sample_every: Duration,
}

/// Counters of a fuzz run.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FuzzStats {
    /// Cases executed (excluding minimization).
    pub cases: u64,
    /// Cases the generator could not build.
    pub build_failures: u64,
    /// Cases where every adapter agreed.
    pub agree: u64,
    /// Diverging cases.
    pub diverge: u64,
    /// Cases with an adapter failure.
    pub adapter_failure: u64,
    /// Cases the reference returned no coverage for.
    pub no_coverage: u64,
    /// Requests run while minimizing.
    pub minimize_runs: u64,
}

/// One point of the coverage timeline.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    /// Seconds since the start.
    pub secs: u64,
    /// Cases executed.
    pub cases: u64,
    /// Distinct features (name and hit-count bucket).
    pub features: u64,
    /// Distinct feature names.
    pub names: u64,
    /// Corpus size.
    pub corpus: u64,
    /// Non-agreeing cases.
    pub findings: u64,
    /// Distinct finding signatures.
    pub signatures: u64,
}

/// Findings that share a signature.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingGroup {
    /// Signature in words.
    pub signature: String,
    /// Cases with this signature.
    pub count: u64,
    /// Recorded finding directories.
    pub recorded: Vec<String>,
    /// Minimized reproducers (request paths) with their transaction counts.
    pub minimized: Vec<(String, usize)>,
}

/// Per-operator mutation statistics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MutationStats {
    /// Children the operator took part in.
    pub tried: u64,
    /// Of those, children kept for new coverage.
    pub kept: u64,
}

/// The `coverage.json` report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport<'a> {
    /// `guided` or `unguided`.
    pub mode: &'static str,
    /// Seconds since the start.
    pub elapsed_secs: f64,
    /// Cases per second.
    pub cases_per_sec: f64,
    /// Counters.
    pub stats: &'a FuzzStats,
    /// Distinct features (name and hit-count bucket).
    pub features: u64,
    /// Distinct feature names.
    pub names: u64,
    /// Distinct names per family.
    pub families: BTreeMap<String, u64>,
    /// Corpus size.
    pub corpus: u64,
    /// Findings per signature.
    pub findings: Vec<&'a FindingGroup>,
    /// Mutation operator statistics.
    pub mutations: &'a BTreeMap<Mutation, MutationStats>,
    /// Coverage over time.
    pub timeline: &'a [Sample],
}

/// A non-agreeing run: the case, its request, and the adapters' answers.
#[derive(Debug, Clone, Copy)]
pub struct Finding<'a> {
    /// The case.
    pub case: &'a FuzzCase,
    /// The built request and its transaction labels.
    pub template: &'a Template,
    /// Every adapter's answer.
    pub results: &'a [(String, Result<Response, CallFailure>)],
    /// The verdict.
    pub verdict: &'a Verdict,
}

/// State the fuzz workers share.
#[derive(Debug)]
pub struct FuzzState {
    /// The corpus and feature statistics.
    pub corpus: Corpus,
    /// Saved or initial cases still to run.
    pub queue: VecDeque<FuzzCase>,
    /// Generator index of the next fresh case.
    pub next_index: u64,
    /// Counters.
    pub stats: FuzzStats,
    /// Findings by signature key.
    pub findings: BTreeMap<(bool, BTreeSet<String>, String), FindingGroup>,
    /// Mutation operator statistics.
    pub mutations: BTreeMap<Mutation, MutationStats>,
    /// Coverage timeline.
    pub timeline: Vec<Sample>,
    /// When `coverage.json` was last written.
    pub last_report: Instant,
    /// When the timeline was last sampled.
    pub last_sample: Instant,
}

/// A coverage-guided differential fuzzer.
#[derive(Debug)]
pub struct Fuzzer {
    /// Settings.
    pub config: FuzzConfig,
}

impl Fuzzer {
    /// The adapter specs to start: the reference with coverage turned on.
    pub fn specs(&self) -> Vec<AdapterSpec> {
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

    fn campaign(&self) -> Campaign {
        Campaign {
            adapters: self.config.adapters.clone(),
            jobs: self.config.jobs,
            timeout: self.config.timeout,
            out: self.config.out.clone(),
            max_findings: self.config.max_findings,
        }
    }

    /// Runs until the duration elapses or enough findings accumulate; returns the final
    /// counters. `coverage.json` in the output directory is rewritten every few seconds.
    pub fn run(&self) -> eyre::Result<FuzzStats> {
        let config = &self.config;
        fs::create_dir_all(config.out.join("findings"))?;
        let (corpus, saved) = Corpus::open(config.corpus.clone())?;
        let mut queue: VecDeque<FuzzCase> = saved.into();
        let mut next_index = 0;
        if queue.is_empty() && config.guided {
            queue.extend(
                (0..config.initial).map(|i| FuzzCase::fresh(config.seed, i, &config.options)),
            );
            next_index = config.initial;
        }
        let started = Instant::now();
        let state = Mutex::new(FuzzState {
            corpus,
            queue,
            next_index,
            stats: FuzzStats::default(),
            findings: BTreeMap::new(),
            mutations: BTreeMap::new(),
            timeline: Vec::new(),
            last_report: started,
            last_sample: started,
        });
        let specs = self.specs();
        thread::scope(|scope| {
            for worker in 0..config.jobs.max(1) {
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
        state: &Mutex<FuzzState>,
        specs: &[AdapterSpec],
        started: Instant,
    ) {
        let config = &self.config;
        let mut adapters: Vec<Adapter> =
            specs.iter().map(|spec| Adapter::new(spec.clone(), config.timeout)).collect();
        let mut rng = Rng::new(&[config.seed, worker, 0xf022_0001]);
        let mut mutator = Mutator {
            rng: Rng::new(&[config.seed, worker, 0xf022_0002]),
            max_slots: config.options.max_txs.max(4) * 2,
            index_space: 1 << 32,
        };
        let reference = adapters.iter().position(|a| a.spec.name == config.reference);
        while started.elapsed() < config.duration {
            let Some((case, mutations)) = self.next_case(state, &mut rng, &mut mutator) else {
                break;
            };
            let Ok(template) = catch_unwind(AssertUnwindSafe(|| case.build(&config.options)))
            else {
                state.lock().unwrap().stats.build_failures += 1;
                continue;
            };
            let (results, verdict) = Minimizer::run(&mut adapters, &template.request);
            let coverage = reference
                .and_then(|i| results[i].1.as_ref().ok())
                .and_then(|response| response.coverage.as_ref());
            let finding = {
                let mut s = state.lock().unwrap();
                let finding = self.account(&mut s, &case, &mutations, coverage, &verdict);
                if s.last_sample.elapsed() >= config.sample_every {
                    Self::sample(&mut s, started);
                }
                if s.last_report.elapsed() >= Duration::from_secs(10) {
                    s.last_report = Instant::now();
                    if let Err(err) = self.report(&s, started) {
                        eprintln!("writing coverage report: {err:#}");
                    }
                }
                finding
            };
            if let Some((signature, minimize)) = finding {
                let finding = Finding {
                    case: &case,
                    template: &template,
                    results: &results,
                    verdict: &verdict,
                };
                self.handle_finding(state, &mut adapters, &finding, &signature, minimize);
            }
        }
    }

    /// The next case to run: a saved corpus case, a fresh generator case, or a mutation of a
    /// corpus case, with the operators applied. `None` once enough findings accumulated.
    fn next_case(
        &self,
        state: &Mutex<FuzzState>,
        rng: &mut Rng,
        mutator: &mut Mutator,
    ) -> Option<(FuzzCase, Vec<Mutation>)> {
        let config = &self.config;
        let mut s = state.lock().unwrap();
        // Known bugs keep reproducing until fixed, so only distinct signatures count.
        if s.findings.len() as u64 >= config.max_findings {
            return None;
        }
        let fresh =
            !config.guided || s.corpus.entries.is_empty() || rng.chance(config.explore_percent);
        Some(match s.queue.pop_front() {
            Some(case) => (case, Vec::new()),
            None if fresh => {
                let index = s.next_index;
                s.next_index += 1;
                (FuzzCase::fresh(config.seed, index, &config.options), Vec::new())
            }
            None => {
                let parent = s.corpus.pick(rng).expect("corpus is not empty");
                let parent = s.corpus.entries[parent].case.clone();
                let donor = s.corpus.donor(rng, &parent);
                drop(s);
                mutator.mutate(&parent, donor.as_ref())
            }
        })
    }

    /// Counts a run, keeps the case if it reached new coverage, and groups a finding by its
    /// signature. Returns the signature when the finding should be recorded, with whether it
    /// should also be minimized.
    fn account(
        &self,
        s: &mut FuzzState,
        case: &FuzzCase,
        mutations: &[Mutation],
        coverage: Option<&BTreeMap<String, u64>>,
        verdict: &Verdict,
    ) -> Option<(FindingSignature, bool)> {
        let config = &self.config;
        s.stats.cases += 1;
        match verdict {
            Verdict::Agree => s.stats.agree += 1,
            Verdict::Diverge { .. } => s.stats.diverge += 1,
            Verdict::AdapterFailure { .. } => s.stats.adapter_failure += 1,
        }
        let kept = match coverage {
            Some(coverage) => {
                let (features, novel) = s.corpus.observe(coverage);
                if novel > 0
                    && let Err(err) = s.corpus.add(case.clone(), features)
                {
                    eprintln!("saving corpus case: {err:#}");
                }
                novel > 0
            }
            None => {
                s.stats.no_coverage += 1;
                false
            }
        };
        for mutation in mutations {
            let entry = s.mutations.entry(*mutation).or_default();
            entry.tried += 1;
            entry.kept += u64::from(kept);
        }
        let signature = FindingSignature::of(verdict, &config.reference)?;
        let group = s.findings.entry(signature.key()).or_default();
        group.signature = signature.describe();
        group.count += 1;
        if group.count == 1 {
            eprintln!("[{}] new finding {}: {}", case.id(), group.signature, verdict.describe());
        }
        let minimize = group.count <= config.minimize_per_signature;
        (group.count <= config.record_per_signature).then_some((signature, minimize))
    }

    /// Records a finding and, if asked, minimizes it with this worker's adapters.
    fn handle_finding(
        &self,
        state: &Mutex<FuzzState>,
        adapters: &mut [Adapter],
        finding: &Finding<'_>,
        signature: &FindingSignature,
        minimize: bool,
    ) {
        let config = &self.config;
        let request = &finding.template.request;
        let line = serde_json::to_string(request).expect("requests serialize");
        if let Err(err) = self.record(finding, &line) {
            eprintln!("recording {}: {err:#}", request.id);
        }
        let dir = config.out.join("findings").join(Self::sanitize(&request.id));
        let key = signature.key();
        let recorded = dir.display().to_string();
        state.lock().unwrap().findings.get_mut(&key).expect("group exists").recorded.push(recorded);
        if !minimize {
            return;
        }
        let mut minimizer = Minimizer {
            adapters,
            reference: &config.reference,
            options: &config.options,
            budget: config.minimize_budget,
            attempts: 0,
        };
        let minimized = minimizer.minimize_case(finding.case, signature);
        let written =
            Self::write_minimized(&dir, &minimized.request, &minimized.verdict, &minimized.results)
                .and_then(|()| {
                    if let Some(case) = &minimized.case {
                        let json = serde_json::to_vec_pretty(case)?;
                        fs::write(dir.join("minimized-case.json"), json)?;
                    }
                    Ok(())
                });
        if let Err(err) = written {
            eprintln!("writing minimized {}: {err:#}", request.id);
        }
        eprintln!(
            "[{}] minimized to {} transactions in {} runs ({})",
            request.id,
            minimized.request.transactions.len(),
            minimized.attempts,
            minimized.verdict.describe()
        );
        let path = dir.join("minimized.json").display().to_string();
        let mut s = state.lock().unwrap();
        s.stats.minimize_runs += minimized.attempts as u64;
        let group = s.findings.get_mut(&key).expect("group exists");
        group.minimized.push((path, minimized.request.transactions.len()));
    }

    /// Writes a minimized request, its verdict, and its responses into a finding directory.
    pub fn write_minimized(
        dir: &std::path::Path,
        request: &Request,
        verdict: &Verdict,
        results: &[(String, Result<Response, CallFailure>)],
    ) -> eyre::Result<()> {
        fs::write(dir.join("minimized.json"), serde_json::to_string(request)?)?;
        fs::write(dir.join("minimized-verdict.json"), serde_json::to_vec_pretty(verdict)?)?;
        for (name, result) in results {
            let body = match result {
                Ok(response) => serde_json::to_vec_pretty(&Self::without_coverage(response))?,
                Err(failure) => failure.to_string().into_bytes(),
            };
            fs::write(dir.join(format!("minimized-{name}.json")), body)?;
        }
        Ok(())
    }

    /// A response without its coverage features, for saved findings.
    pub fn without_coverage(response: &Response) -> Response {
        Response { coverage: None, ..response.clone() }
    }

    fn record(&self, finding: &Finding<'_>, line: &str) -> eyre::Result<()> {
        let request = &finding.template.request;
        let results: Vec<_> = finding
            .results
            .iter()
            .map(|(name, result)| {
                (name.clone(), result.as_ref().map(Self::without_coverage).map_err(Clone::clone))
            })
            .collect();
        self.campaign().record(request, line, &results, finding.verdict)?;
        let dir = self.config.out.join("findings").join(Self::sanitize(&request.id));
        fs::write(dir.join("case.json"), serde_json::to_vec_pretty(finding.case)?)?;
        fs::write(dir.join("labels.json"), serde_json::to_vec_pretty(&finding.template.labels)?)?;
        Ok(())
    }

    /// The directory name of a case id.
    pub fn sanitize(id: &str) -> String {
        id.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect()
    }

    fn sample(s: &mut FuzzState, started: Instant) {
        s.last_sample = Instant::now();
        let sample = Sample {
            secs: started.elapsed().as_secs(),
            cases: s.stats.cases,
            features: s.corpus.ids.len() as u64,
            names: s.corpus.names.len() as u64,
            corpus: s.corpus.entries.len() as u64,
            findings: s.stats.diverge + s.stats.adapter_failure,
            signatures: s.findings.len() as u64,
        };
        eprintln!(
            "[{}s] cases {} features {} names {} corpus {} findings {} ({} signatures)",
            sample.secs,
            sample.cases,
            sample.features,
            sample.names,
            sample.corpus,
            sample.findings,
            sample.signatures
        );
        s.timeline.push(sample);
    }

    fn report(&self, s: &FuzzState, started: Instant) -> eyre::Result<()> {
        let elapsed = started.elapsed().as_secs_f64();
        let mut findings: Vec<&FindingGroup> = s.findings.values().collect();
        findings.sort_by_key(|group| std::cmp::Reverse(group.count));
        let report = CoverageReport {
            mode: if self.config.guided { "guided" } else { "unguided" },
            elapsed_secs: elapsed,
            cases_per_sec: s.stats.cases as f64 / elapsed.max(1e-9),
            stats: &s.stats,
            features: s.corpus.ids.len() as u64,
            names: s.corpus.names.len() as u64,
            families: s.corpus.families(),
            corpus: s.corpus.entries.len() as u64,
            findings,
            mutations: &s.mutations,
            timeline: &s.timeline,
        };
        let path = self.config.out.join("coverage.json");
        let tmp = self.config.out.join("coverage.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&report)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}
