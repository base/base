//! Drives multi-block synthetic chains through every adapter.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use serde::Serialize;

use crate::{
    Adapter, AdapterSpec, Campaign, ChainGenerator, Era, FindingSignature, Fuzzer,
    GeneratorOptions, Minimizer, Request, Verdict,
};

/// Settings of a chain run.
#[derive(Debug, Clone)]
pub struct ChainDriver {
    /// Adapters to compare.
    pub adapters: Vec<AdapterSpec>,
    /// Reference client: its response advances the chain after agreement.
    pub reference: String,
    /// Worker threads; each runs one chain at a time with one process of every adapter.
    pub jobs: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Output directory.
    pub out: PathBuf,
    /// Base seed; chain `i` uses seed `seed * 1_000_000 + i`.
    pub seed: u64,
    /// Chains to run.
    pub chains: u64,
    /// Blocks per chain.
    pub length: u64,
    /// Generator options.
    pub options: GeneratorOptions,
    /// Candidate requests a minimization may run (0 disables minimization).
    pub minimize_budget: usize,
}

/// Counters of a chain run.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainSummary {
    /// Chains run.
    pub chains: u64,
    /// Blocks every adapter agreed on.
    pub agreed_blocks: u64,
    /// Chains that ended in a divergence.
    pub diverge: u64,
    /// Chains that ended in an adapter failure.
    pub adapter_failure: u64,
    /// Chains that reached their length.
    pub completed: u64,
    /// Why other chains stopped early (generator errors), with counts.
    pub stopped: BTreeMap<String, u64>,
    /// Finding directories.
    pub findings: Vec<String>,
    /// Agreed blocks by the era they executed in.
    pub agreed_by_era: BTreeMap<Era, u64>,
    /// Chains whose agreed blocks span a fork activation, by the first era they entered.
    pub crossings: BTreeMap<Era, u64>,
    /// The longest chains, longest first.
    pub longest: Vec<ChainRecord>,
}

/// One chain in the summary.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainRecord {
    /// Chain seed.
    pub seed: u64,
    /// Agreed blocks.
    pub blocks: u64,
    /// Number of the first block.
    pub start: u64,
    /// Era of the first block.
    pub first_era: Era,
    /// Era of the last agreed block.
    pub last_era: Era,
    /// How it ended.
    pub end: String,
}

impl ChainSummary {
    /// Longest chains the summary keeps.
    pub const LONGEST: usize = 20;
}

impl ChainDriver {
    /// Requests kept for `prefix.jsonl`. Requests carry full witnesses, so keeping a whole
    /// 1,000-block chain per worker exhausts memory.
    pub const PREFIX_LEN: usize = 32;

    /// Runs the chains and returns the summary, also written to `<out>/chain-summary.json`.
    ///
    /// A chain advances only when every adapter agrees, on the reference's response. On a
    /// divergence or failure the chain stops and the finding directory gets `prefix.jsonl`, the last
    /// [`Self::PREFIX_LEN`] requests of the chain up to and including the failing one (each replays
    /// on its own; the prefix shows how the state was reached), plus the usual request, verdict, and responses,
    /// and a minimized `minimized.json` of the failing block.
    pub fn run(&self) -> eyre::Result<ChainSummary> {
        eyre::ensure!(
            self.adapters.iter().any(|a| a.name == self.reference),
            "reference adapter {} is not among the adapters",
            self.reference
        );
        fs::create_dir_all(self.out.join("findings"))?;
        let next = AtomicU64::new(0);
        let summary = Mutex::new(ChainSummary::default());
        let campaign = Campaign {
            adapters: self.adapters.clone(),
            jobs: self.jobs,
            timeout: self.timeout,
            out: self.out.clone(),
            max_findings: u64::MAX,
        };
        thread::scope(|scope| {
            for _ in 0..self.jobs.max(1) {
                scope.spawn(|| {
                    let mut adapters: Vec<Adapter> = self
                        .adapters
                        .iter()
                        .map(|spec| Adapter::new(spec.clone(), self.timeout))
                        .collect();
                    loop {
                        let chain = next.fetch_add(1, Ordering::Relaxed);
                        if chain >= self.chains {
                            break;
                        }
                        let seed = self.seed.wrapping_mul(1_000_000).wrapping_add(chain);
                        let outcome = self.chain(seed, &mut adapters, &campaign);
                        let mut s = summary.lock().unwrap();
                        s.chains += 1;
                        s.agreed_blocks += outcome.agreed;
                        for (era, blocks) in &outcome.eras {
                            *s.agreed_by_era.entry(*era).or_default() += blocks;
                        }
                        for era in outcome.eras.keys().skip(1) {
                            *s.crossings.entry(*era).or_default() += 1;
                        }
                        let end = match &outcome.end {
                            ChainEnd::Completed => "completed".to_owned(),
                            ChainEnd::Stopped(why) => format!("stopped: {why}"),
                            ChainEnd::Finding { dir, .. } => format!("finding: {dir}"),
                        };
                        if let (Some(first_era), Some(last_era)) =
                            (outcome.eras.keys().next(), outcome.eras.keys().next_back())
                        {
                            s.longest.push(ChainRecord {
                                seed,
                                blocks: outcome.agreed,
                                start: outcome.start,
                                first_era: *first_era,
                                last_era: *last_era,
                                end,
                            });
                            s.longest.sort_by(|a, b| b.blocks.cmp(&a.blocks));
                            s.longest.truncate(ChainSummary::LONGEST);
                        }
                        match outcome.end {
                            ChainEnd::Completed => s.completed += 1,
                            ChainEnd::Stopped(why) => *s.stopped.entry(why).or_default() += 1,
                            ChainEnd::Finding { failure, dir } => {
                                if failure {
                                    s.adapter_failure += 1
                                } else {
                                    s.diverge += 1
                                }
                                s.findings.push(dir);
                            }
                        }
                        eprintln!(
                            "chains {}/{}: agreed blocks {} diverge {} failure {} completed {}",
                            s.chains,
                            self.chains,
                            s.agreed_blocks,
                            s.diverge,
                            s.adapter_failure,
                            s.completed
                        );
                        let _ = fs::write(
                            self.out.join("chain-summary.json"),
                            serde_json::to_vec_pretty(&*s).unwrap_or_default(),
                        );
                    }
                });
            }
        });
        let summary = summary.into_inner().unwrap();
        fs::write(self.out.join("chain-summary.json"), serde_json::to_vec_pretty(&summary)?)?;
        Ok(summary)
    }

    fn chain(&self, seed: u64, adapters: &mut [Adapter], campaign: &Campaign) -> ChainOutcome {
        let built =
            catch_unwind(AssertUnwindSafe(|| ChainGenerator::new(seed, self.options.clone())));
        let Ok(mut chain) = built else {
            return ChainOutcome {
                agreed: 0,
                start: 0,
                eras: BTreeMap::new(),
                end: ChainEnd::Stopped("generator panicked".into()),
            };
        };
        let start = chain.request().header.number;
        let mut eras: BTreeMap<Era, u64> = BTreeMap::new();
        let mut prefix: VecDeque<Request> = VecDeque::with_capacity(Self::PREFIX_LEN);
        let reference = adapters
            .iter()
            .position(|a| a.spec.name == self.reference)
            .expect("run checks the reference");
        while chain.blocks() < self.length {
            let request = chain.request().clone();
            if prefix.len() == Self::PREFIX_LEN {
                prefix.pop_front();
            }
            prefix.push_back(request.clone());
            let (results, verdict) = Minimizer::run(adapters, &request);
            if let Some(signature) = FindingSignature::of(&verdict, &self.reference) {
                eprintln!("{}: {}", request.id, verdict.describe());
                let dir = self.record(
                    prefix.make_contiguous(),
                    &results,
                    &verdict,
                    &signature,
                    adapters,
                    campaign,
                );
                return ChainOutcome {
                    agreed: chain.blocks(),
                    start,
                    eras,
                    end: ChainEnd::Finding { failure: signature.failure, dir },
                };
            }
            let Ok(response) = &results[reference].1 else {
                unreachable!("agreement needs responses")
            };
            *eras.entry(Era::at(request.header.timestamp)).or_default() += 1;
            match catch_unwind(AssertUnwindSafe(|| chain.next(response))) {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    let why: String = format!("{err:#}").chars().take(120).collect();
                    // Keep the chain so the generator gap can be reproduced.
                    let dir = self.out.join("stopped").join(Fuzzer::sanitize(&request.id));
                    let saved = fs::create_dir_all(&dir).and_then(|()| {
                        let mut jsonl = String::new();
                        for request in &prefix {
                            jsonl.push_str(&serde_json::to_string(request).unwrap_or_default());
                            jsonl.push('\n');
                        }
                        fs::write(dir.join("prefix.jsonl"), jsonl)?;
                        fs::write(
                            dir.join("response.json"),
                            serde_json::to_vec_pretty(response).unwrap_or_default(),
                        )
                    });
                    if let Err(err) = saved {
                        eprintln!("saving stopped chain {}: {err:#}", request.id);
                    }
                    return ChainOutcome {
                        agreed: chain.blocks(),
                        start,
                        eras,
                        end: ChainEnd::Stopped(why),
                    };
                }
                Err(_) => {
                    return ChainOutcome {
                        agreed: chain.blocks(),
                        start,
                        eras,
                        end: ChainEnd::Stopped("generator panicked".into()),
                    };
                }
            }
        }
        ChainOutcome { agreed: chain.blocks(), start, eras, end: ChainEnd::Completed }
    }

    fn record(
        &self,
        prefix: &[Request],
        results: &[(String, Result<crate::Response, crate::CallFailure>)],
        verdict: &Verdict,
        signature: &FindingSignature,
        adapters: &mut [Adapter],
        campaign: &Campaign,
    ) -> String {
        let request = prefix.last().expect("prefix holds the failing request");
        let line = serde_json::to_string(request).expect("requests serialize");
        let dir = self.out.join("findings").join(Fuzzer::sanitize(&request.id));
        let written = campaign.record(request, &line, results, verdict).and_then(|()| {
            let mut jsonl = String::new();
            for request in prefix {
                jsonl.push_str(&serde_json::to_string(request)?);
                jsonl.push('\n');
            }
            fs::write(dir.join("prefix.jsonl"), jsonl)?;
            Ok(())
        });
        if let Err(err) = written {
            eprintln!("recording {}: {err:#}", request.id);
        }
        if self.minimize_budget > 0 {
            let mut minimizer = Minimizer {
                adapters,
                reference: &self.reference,
                options: &self.options,
                budget: self.minimize_budget,
                attempts: 0,
            };
            let mut reduced = request.clone();
            reduced.id = format!("{}-min", request.id);
            let minimized = minimizer.minimize_request(&reduced, signature);
            if let Err(err) = Fuzzer::write_minimized(
                &dir,
                &minimized.request,
                &minimized.verdict,
                &minimized.results,
            ) {
                eprintln!("writing minimized {}: {err:#}", request.id);
            }
        }
        dir.display().to_string()
    }
}

/// How one chain ended.
#[derive(Debug)]
pub struct ChainOutcome {
    /// Blocks agreed before the end.
    pub agreed: u64,
    /// Number of the first block.
    pub start: u64,
    /// Agreed blocks by the era they executed in.
    pub eras: BTreeMap<Era, u64>,
    /// The end.
    pub end: ChainEnd,
}

/// Why a chain ended.
#[derive(Debug)]
pub enum ChainEnd {
    /// It reached its length.
    Completed,
    /// The generator could not continue.
    Stopped(String),
    /// The adapters disagreed or one failed.
    Finding {
        /// Whether an adapter failed.
        failure: bool,
        /// Finding directory.
        dir: String,
    },
}
