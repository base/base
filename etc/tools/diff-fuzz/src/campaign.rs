//! Runs many cases on every adapter in parallel and records the verdicts.

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{Adapter, AdapterSpec, CallFailure, Request, Response, Verdict};

/// Counts of verdicts in a campaign.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    /// Cases attempted.
    pub cases: u64,
    /// Cases the source could not produce (for example a mainnet block whose witness is
    /// unavailable).
    pub skipped: u64,
    /// Cases where every adapter agreed.
    pub agree: u64,
    /// Cases with at least one differing field.
    pub diverge: u64,
    /// Cases where an adapter failed.
    pub adapter_failure: u64,
    /// Transactions in agreeing cases.
    pub transactions: u64,
    /// Rejected transactions in agreeing cases (as reported by the first adapter).
    pub rejected: u64,
    /// Number of times each differing field appeared, across diverging cases.
    pub fields: BTreeMap<String, u64>,
    /// Failure counts per adapter.
    pub failures: BTreeMap<String, u64>,
}

/// A set of adapters, a case source, and where to record results.
#[derive(Debug, Clone)]
pub struct Campaign {
    /// Adapters to compare.
    pub adapters: Vec<AdapterSpec>,
    /// Worker threads; each owns one process of every adapter.
    pub jobs: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Output directory for divergences, failures, and the summary.
    pub out: PathBuf,
    /// Stop after this many diverging or failing cases.
    pub max_findings: u64,
}

impl Campaign {
    /// Runs cases `0..count` produced by `source` and returns the summary. `source` returns
    /// `None` to skip a case. The summary is also written to `<out>/summary.json` as it changes.
    pub fn run<F>(&self, count: u64, source: F) -> eyre::Result<Summary>
    where
        F: Fn(u64) -> eyre::Result<Option<Request>> + Sync,
    {
        fs::create_dir_all(self.out.join("findings"))?;
        let next = AtomicU64::new(0);
        let summary = Mutex::new(Summary::default());
        let findings = AtomicU64::new(0);
        let started = Instant::now();
        thread::scope(|scope| {
            for _ in 0..self.jobs.max(1) {
                scope.spawn(|| {
                    let mut adapters: Vec<Adapter> = self
                        .adapters
                        .iter()
                        .map(|spec| Adapter::new(spec.clone(), self.timeout))
                        .collect();
                    loop {
                        if findings.load(Ordering::Relaxed) >= self.max_findings {
                            break;
                        }
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= count {
                            break;
                        }
                        let request = match source(index) {
                            Ok(Some(request)) => request,
                            Ok(None) => {
                                summary.lock().unwrap().skipped += 1;
                                continue;
                            }
                            Err(err) => {
                                eprintln!("case {index}: source failed: {err:#}");
                                summary.lock().unwrap().skipped += 1;
                                continue;
                            }
                        };
                        let line = serde_json::to_string(&request).expect("requests serialize");
                        let results: Vec<_> = adapters
                            .iter_mut()
                            .map(|adapter| {
                                (adapter.spec.name.clone(), adapter.call_line(&request.id, &line))
                            })
                            .collect();
                        let verdict = Verdict::of(&request, &results);
                        let mut s = summary.lock().unwrap();
                        s.cases += 1;
                        match &verdict {
                            Verdict::Agree => {
                                s.agree += 1;
                                s.transactions += request.transactions.len() as u64;
                                if let Some((_, Ok(first))) = results.first() {
                                    s.rejected += first.rejected.len() as u64;
                                }
                            }
                            Verdict::Diverge { differences } => {
                                s.diverge += 1;
                                for difference in differences {
                                    *s.fields.entry(difference.field.clone()).or_default() += 1;
                                }
                            }
                            Verdict::AdapterFailure { failures } => {
                                s.adapter_failure += 1;
                                for name in failures.keys() {
                                    *s.failures.entry(name.clone()).or_default() += 1;
                                }
                            }
                        }
                        let done = s.cases;
                        let snapshot = s.clone();
                        drop(s);
                        if verdict != Verdict::Agree {
                            findings.fetch_add(1, Ordering::Relaxed);
                            if let Err(err) = self.record(&request, &line, &results, &verdict) {
                                eprintln!("recording {}: {err:#}", request.id);
                            }
                            eprintln!("{}: {}", request.id, verdict.describe());
                        }
                        if done % 100 == 0 || verdict != Verdict::Agree {
                            let rate = done as f64 / started.elapsed().as_secs_f64();
                            eprintln!(
                                "[{done}/{count}] agree {} diverge {} failure {} skipped {} ({rate:.1} cases/s)",
                                snapshot.agree,
                                snapshot.diverge,
                                snapshot.adapter_failure,
                                snapshot.skipped
                            );
                            let _ = self.write_summary(&snapshot);
                        }
                    }
                });
            }
        });
        let summary = summary.into_inner().unwrap();
        self.write_summary(&summary)?;
        Ok(summary)
    }

    fn write_summary(&self, summary: &Summary) -> eyre::Result<()> {
        fs::write(self.out.join("summary.json"), serde_json::to_vec_pretty(summary)?)?;
        Ok(())
    }

    /// Writes a finding to `<out>/findings/<id>/`: the request, the verdict, and every
    /// adapter's response or failure.
    pub fn record(
        &self,
        request: &Request,
        line: &str,
        results: &[(String, Result<Response, CallFailure>)],
        verdict: &Verdict,
    ) -> eyre::Result<()> {
        let dir = self.out.join("findings").join(sanitize(&request.id));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("request.json"), line)?;
        fs::write(dir.join("verdict.json"), serde_json::to_vec_pretty(verdict)?)?;
        for (name, result) in results {
            let body = match result {
                Ok(response) => serde_json::to_vec_pretty(response)?,
                Err(failure) => failure.to_string().into_bytes(),
            };
            fs::write(dir.join(format!("{name}.json")), body)?;
        }
        Ok(())
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
