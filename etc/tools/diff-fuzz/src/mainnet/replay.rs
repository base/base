//! Validate-mode replay of Base mainnet blocks on every adapter, tallied per fork era.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use super::MainnetFetcher;
use crate::{Adapter, AdapterSpec, Campaign, Era, Verdict, era_blocks};

/// Outcome of one replayed block, one line of `<out>/results.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockResult {
    /// Block number.
    pub block: u64,
    /// Fork era of the block.
    pub era: Era,
    /// Transactions in the block (0 when skipped).
    pub transactions: u64,
    /// `agree`, `diverge`, `failure`, or `skipped`.
    pub verdict: String,
    /// Differing fields, adapter failures, or the reason the block was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the block ran on a synthetic pre-state (see [`super::PrestateWitness`]), where
    /// the state root, withdrawals root and hash are only compared between clients.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prestate: bool,
}

/// Per-era counts of replayed blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EraTally {
    /// Blocks attempted.
    pub blocks: u64,
    /// Blocks where every adapter matched the canonical header and each other.
    pub agree: u64,
    /// Transactions in agreeing blocks.
    pub transactions: u64,
    /// Diverging blocks.
    pub diverge: u64,
    /// Blocks where an adapter failed.
    pub failure: u64,
    /// Blocks that could not be fetched.
    pub skipped: u64,
}

/// Replays mainnet blocks in validate mode on every adapter.
///
/// Results are appended to `<out>/results.jsonl` as they come in, so a rerun with the same
/// output directory skips blocks that already agreed and reruns the rest (for example after a
/// client fix). Divergences and failures are recorded like [`Campaign`] findings, and
/// `<out>/eras.json` holds the per-era tally of the latest result of every block in the file.
#[derive(Debug, Clone)]
pub struct MainnetReplay {
    /// Block source.
    pub fetcher: MainnetFetcher,
    /// Adapters to compare.
    pub adapters: Vec<AdapterSpec>,
    /// Worker threads; each fetches blocks and runs one process of every adapter.
    pub jobs: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Output directory.
    pub out: PathBuf,
}

impl MainnetReplay {
    /// Parses a block list such as `9101527,58500000..58500100` (ranges are half-open).
    pub fn parse_blocks(spec: &str) -> eyre::Result<Vec<u64>> {
        let mut blocks = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some((a, b)) = part.split_once("..") {
                blocks.extend(a.parse::<u64>()?..b.parse::<u64>()?);
            } else {
                blocks.push(part.parse()?);
            }
        }
        Ok(blocks)
    }

    /// `per_era` blocks spread evenly over each era from `first_era` on, plus every activation
    /// block and the block after it. The Denim era ends at `tip`.
    pub fn era_samples(per_era: u64, first_era: Era, tip: u64) -> Vec<u64> {
        let mut blocks = BTreeSet::new();
        for era in Era::ALL.into_iter().filter(|era| *era >= first_era) {
            let (first, last) = era_blocks(era);
            let last = last.min(tip);
            if first > last {
                continue;
            }
            blocks.insert(first);
            blocks.insert(first + 1);
            let span = last - first;
            for i in 1..=per_era {
                blocks.insert(first + span * i / (per_era + 1));
            }
        }
        blocks.into_iter().collect()
    }

    /// Replays `blocks` and returns the per-era tally of everything in `results.jsonl`.
    pub fn run(&self, blocks: &[u64]) -> eyre::Result<BTreeMap<Era, EraTally>> {
        fs::create_dir_all(self.out.join("findings"))?;
        let results_path = self.out.join("results.jsonl");
        let done: BTreeSet<u64> = Self::load(&results_path)?
            .into_iter()
            .filter(|r| r.verdict == "agree")
            .map(|r| r.block)
            .collect();
        let todo: Vec<u64> = blocks.iter().copied().filter(|b| !done.contains(b)).collect();
        eprintln!("{} blocks to replay ({} already done)", todo.len(), blocks.len() - todo.len());
        let campaign = Campaign {
            adapters: self.adapters.clone(),
            jobs: self.jobs,
            timeout: self.timeout,
            out: self.out.clone(),
            max_findings: u64::MAX,
        };
        let file = Mutex::new(OpenOptions::new().create(true).append(true).open(&results_path)?);
        let next = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..self.jobs.max(1) {
                scope.spawn(|| {
                    let mut adapters: Vec<Adapter> = self
                        .adapters
                        .iter()
                        .map(|spec| Adapter::new(spec.clone(), self.timeout))
                        .collect();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&block) = todo.get(i) else { break };
                        let result = self.replay_one(&campaign, &mut adapters, block);
                        eprintln!(
                            "[{}/{}] block {block} ({:?}, {} txs): {} {}",
                            i + 1,
                            todo.len(),
                            result.era,
                            result.transactions,
                            result.verdict,
                            result.detail.as_deref().unwrap_or("")
                        );
                        let line = serde_json::to_string(&result).expect("results serialize");
                        let mut file = file.lock().unwrap();
                        let _ = writeln!(file, "{line}");
                    }
                });
            }
        });
        let tally = Self::tally(&Self::load(&results_path)?);
        fs::write(self.out.join("eras.json"), serde_json::to_vec_pretty(&tally)?)?;
        Ok(tally)
    }

    fn replay_one(&self, campaign: &Campaign, adapters: &mut [Adapter], block: u64) -> BlockResult {
        let era_of_number = Era::ALL
            .into_iter()
            .rev()
            .find(|era| era_blocks(*era).0 <= block)
            .unwrap_or(Era::Regolith);
        let request = match self.fetcher.fetch_case(block) {
            Ok(request) => request,
            Err(error) => {
                return BlockResult {
                    block,
                    era: era_of_number,
                    transactions: 0,
                    verdict: "skipped".to_owned(),
                    detail: Some(format!("{error:#}").chars().take(300).collect()),
                    prestate: self.fetcher.witness_from_prestate,
                };
            }
        };
        // A synthetic pre-state request runs in build mode without the canonical header; the
        // canonical values that depend on the real parent state (state root, withdrawals root,
        // hash) are then taken from the first client, so they are only compared between clients.
        let prestate = request.witness.root.is_some();
        let mut sent = request.clone();
        if prestate {
            sent.header.results = None;
        }
        let line = serde_json::to_string(&sent).expect("requests serialize");
        let results: Vec<_> = adapters
            .iter_mut()
            .map(|adapter| (adapter.spec.name.clone(), adapter.call_line(&request.id, &line)))
            .collect();
        let mut compared = request.clone();
        if prestate
            && let Some(canonical) = compared.header.results.as_mut()
            && let Some(Ok(computed)) =
                results.first().map(|(_, r)| r.as_ref().map(|r| r.header.clone()))
            && let Some(computed) = computed
        {
            canonical.state_root = computed.state_root;
            canonical.withdrawals_root = computed.withdrawals_root;
            canonical.hash = computed.hash;
        }
        let verdict = Verdict::of(&compared, &results);
        if verdict != Verdict::Agree
            && let Err(error) = campaign.record(&sent, &line, &results, &verdict)
        {
            eprintln!("recording block {block}: {error:#}");
        }
        let (name, detail) = match &verdict {
            Verdict::Agree => ("agree", None),
            Verdict::Diverge { .. } => ("diverge", Some(verdict.describe())),
            Verdict::AdapterFailure { .. } => ("failure", Some(verdict.describe())),
        };
        BlockResult {
            block,
            era: Era::at(request.header.timestamp),
            transactions: request.transactions.len() as u64,
            verdict: name.to_owned(),
            detail,
            prestate,
        }
    }

    /// Reads `results.jsonl`; a later line for a block replaces an earlier one.
    pub fn load(path: &PathBuf) -> eyre::Result<Vec<BlockResult>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut by_block = BTreeMap::new();
        for line in fs::read_to_string(path)?.lines().filter(|l| !l.trim().is_empty()) {
            let result: BlockResult = serde_json::from_str(line)?;
            by_block.insert(result.block, result);
        }
        Ok(by_block.into_values().collect())
    }

    /// Per-era counts of `results`.
    pub fn tally(results: &[BlockResult]) -> BTreeMap<Era, EraTally> {
        let mut tally: BTreeMap<Era, EraTally> = BTreeMap::new();
        for result in results {
            let entry = tally.entry(result.era).or_default();
            entry.blocks += 1;
            match result.verdict.as_str() {
                "agree" => {
                    entry.agree += 1;
                    entry.transactions += result.transactions;
                }
                "diverge" => entry.diverge += 1,
                "failure" => entry.failure += 1,
                _ => entry.skipped += 1,
            }
        }
        tally
    }
}
