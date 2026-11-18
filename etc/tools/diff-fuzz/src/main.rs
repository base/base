//! `diff-fuzz` command-line entry point.

use std::{fs, path::PathBuf, time::Duration};

use base_diff_fuzz::{
    AdapterSpec, Campaign, ChainDriver, ConfigLogs, Era, FuzzConfig, Fuzzer, Generator,
    GeneratorOptions, MainnetFetcher, MainnetReplay, Request,
};
use clap::{Args, Parser, Subcommand};
use eyre::WrapErr;

#[derive(Debug, Parser)]
#[command(about = "Differential fuzzing and mainnet replay across Base execution clients")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Common {
    /// Adapter as `name=command args...`; repeat for each client.
    #[arg(long = "adapter", required = true)]
    adapters: Vec<String>,
    /// Worker threads; each runs one process of every adapter.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
    /// Output directory.
    #[arg(long, default_value = "diff-fuzz-out")]
    out: PathBuf,
    /// Stop after this many findings (for `fuzz`: distinct finding signatures).
    #[arg(long, default_value_t = 1000)]
    max_findings: u64,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run saved requests: JSON files (one request each) or JSON-lines files, or directories of
    /// them.
    Run {
        #[command(flatten)]
        common: Common,
        /// Request files or directories.
        paths: Vec<PathBuf>,
    },
    /// Coverage-guided fuzzing: mutates corpus cases, keeps the ones that reach new coverage
    /// in the `--coverage-from` adapters, and records (and minimizes) every divergence or failure.
    Fuzz {
        #[command(flatten)]
        common: Common,
        /// Corpus directory; cases found there are run first.
        #[arg(long)]
        corpus: PathBuf,
        /// How long to run, such as `90s`, `20m`, or `2h`.
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        duration: Duration,
        /// Generator seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Reference adapter that findings are compared against.
        #[arg(long, default_value = "base")]
        reference: String,
        /// Adapters started with `BASE_DIFF_COVERAGE=1` whose features are merged; repeat for
        /// several. Defaults to the reference.
        #[arg(long)]
        coverage_from: Vec<String>,
        /// Fresh cases to start from when the corpus is empty.
        #[arg(long, default_value_t = 64)]
        initial: u64,
        /// Percentage of rounds that run a fresh case instead of a mutation.
        #[arg(long, default_value_t = 20)]
        explore: u64,
        /// Only run fresh generator cases (the unguided baseline); coverage is still measured.
        #[arg(long)]
        unguided: bool,
        /// Candidate requests one minimization may run (0 disables minimization).
        #[arg(long, default_value_t = 120)]
        minimize_budget: usize,
        /// Seconds between coverage timeline samples.
        #[arg(long, default_value_t = 60)]
        sample_secs: u64,
    },
    /// Single-block generator cases `0..count` of a seed.
    Gen {
        #[command(flatten)]
        common: Common,
        /// Generator seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Number of cases.
        #[arg(long, default_value_t = 1000)]
        count: u64,
    },
    /// `systemConfig` cases `0..count`: L1 `ConfigUpdate` logs, well-formed and malformed,
    /// applied to a system config.
    SystemConfig {
        #[command(flatten)]
        common: Common,
        /// Generator seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Number of cases.
        #[arg(long, default_value_t = 10_000)]
        count: u64,
    },
    /// Validate-mode replay of Base mainnet blocks, tallied per fork era.
    Mainnet {
        #[command(flatten)]
        common: Common,
        /// Directory caching fetched blocks as `<number>.json`.
        #[arg(long, default_value = "mainnet-cache")]
        cache: PathBuf,
        /// Blocks, such as `9101527,58500000..58500100` (half-open ranges).
        #[arg(long, default_value = "")]
        blocks: String,
        /// The last N blocks before the chain head (minus a 20-block margin).
        #[arg(long, default_value_t = 0)]
        tip: u64,
        /// Blocks to sample evenly from each era, besides its activation blocks.
        #[arg(long, default_value_t = 0)]
        per_era: u64,
        /// Oldest era to sample.
        #[arg(long, default_value = "regolith")]
        first_era: String,
        /// Blocks fetched in parallel before replaying (0 fetches inside the workers).
        #[arg(long, default_value_t = 16)]
        fetch_jobs: usize,
        /// Only replay cached blocks.
        #[arg(long)]
        offline: bool,
    },
    /// Multi-block synthetic chains; each advances only while every adapter agrees.
    Chain {
        #[command(flatten)]
        common: Common,
        /// Base seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Number of chains.
        #[arg(long, default_value_t = 16)]
        chains: u64,
        /// Blocks per chain.
        #[arg(long, default_value_t = 50)]
        length: u64,
        /// Reference adapter whose response advances the chain.
        #[arg(long, default_value = "base")]
        reference: String,
        /// Candidate requests one minimization may run (0 disables minimization).
        #[arg(long, default_value_t = 60)]
        minimize_budget: usize,
    },
}

/// Parses `90`, `90s`, `20m`, or `2h`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let (number, unit) =
        text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let number: u64 = number.parse().map_err(|_| format!("invalid duration {text:?}"))?;
    let secs = match unit {
        "" | "s" => number,
        "m" => number * 60,
        "h" => number * 3600,
        _ => return Err(format!("invalid duration unit in {text:?}")),
    };
    Ok(Duration::from_secs(secs))
}

impl Common {
    fn campaign(&self) -> eyre::Result<Campaign> {
        Ok(Campaign {
            adapters: self
                .adapters
                .iter()
                .map(|s| AdapterSpec::parse(s))
                .collect::<eyre::Result<_>>()?,
            jobs: self.jobs,
            timeout: Duration::from_secs(self.timeout),
            out: self.out.clone(),
            max_findings: self.max_findings,
        })
    }
}

/// A saved request: a whole file, or one line of a JSON-lines file.
#[derive(Debug, Clone)]
struct Saved {
    path: PathBuf,
    line: Option<usize>,
}

impl Saved {
    fn collect(paths: &[PathBuf]) -> eyre::Result<Vec<Self>> {
        let mut out = Vec::new();
        for path in paths {
            if path.is_dir() {
                let mut entries: Vec<PathBuf> =
                    fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
                entries.sort();
                out.extend(Self::collect(&entries)?);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                let lines =
                    fs::read_to_string(path)?.lines().filter(|l| !l.trim().is_empty()).count();
                out.extend((0..lines).map(|i| Self { path: path.clone(), line: Some(i) }));
            } else if path.extension().is_some_and(|e| e == "json") {
                out.push(Self { path: path.clone(), line: None });
            }
        }
        Ok(out)
    }

    fn load(&self) -> eyre::Result<Request> {
        let text = fs::read_to_string(&self.path)?;
        let body = self.line.map_or(text.as_str(), |i| {
            text.lines().filter(|l| !l.trim().is_empty()).nth(i).unwrap_or_default()
        });
        serde_json::from_str(body).wrap_err_with(|| format!("parsing {}", self.path.display()))
    }
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run { common, paths } => {
            let saved = Saved::collect(&paths)?;
            let summary = common
                .campaign()?
                .run(saved.len() as u64, |i| saved[i as usize].load().map(Some))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::Fuzz {
            common,
            corpus,
            duration,
            seed,
            reference,
            coverage_from,
            initial,
            explore,
            unguided,
            minimize_budget,
            sample_secs,
        } => {
            let campaign = common.campaign()?;
            let fuzzer = Fuzzer {
                config: FuzzConfig {
                    adapters: campaign.adapters,
                    coverage_from: if coverage_from.is_empty() {
                        vec![reference.clone()]
                    } else {
                        coverage_from
                    },
                    reference,
                    jobs: campaign.jobs,
                    timeout: campaign.timeout,
                    out: campaign.out,
                    corpus: Some(corpus),
                    duration,
                    seed,
                    options: GeneratorOptions::default(),
                    guided: !unguided,
                    initial,
                    explore_percent: explore,
                    record_per_signature: 5,
                    minimize_per_signature: 2,
                    minimize_budget,
                    max_findings: campaign.max_findings,
                    sample_every: Duration::from_secs(sample_secs),
                },
            };
            let stats = fuzzer.run()?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::Gen { common, seed, count } => {
            let generator = Generator::new(seed, GeneratorOptions::default());
            let summary = common.campaign()?.run(count, |i| Ok(Some(generator.case(i))))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::SystemConfig { common, seed, count } => {
            let summary =
                common.campaign()?.run(count, |i| Ok(Some(ConfigLogs::request(seed, i))))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::Mainnet {
            common,
            cache,
            blocks,
            tip,
            per_era,
            first_era,
            fetch_jobs,
            offline,
        } => {
            let mut fetcher = MainnetFetcher::from_env(cache)?;
            fetcher.offline = offline;
            let mut list = MainnetReplay::parse_blocks(&blocks)?;
            if tip > 0 || per_era > 0 {
                let head: String = serde_json::from_value(
                    fetcher.blocks.call("eth_blockNumber", serde_json::json!([]))?,
                )?;
                let head = u64::from_str_radix(head.trim_start_matches("0x"), 16)? - 20;
                list.extend(head.saturating_sub(tip)..head);
                if per_era > 0 {
                    let first: Era = serde_json::from_value(serde_json::json!(first_era))?;
                    list.extend(MainnetReplay::era_samples(per_era, first, head));
                }
            }
            list.sort_unstable();
            list.dedup();
            if fetch_jobs > 0 && !offline {
                let missing: Vec<u64> =
                    list.iter().copied().filter(|b| !fetcher.cache_path(*b).exists()).collect();
                eprintln!("prefetching {} blocks", missing.len());
                std::thread::scope(|scope| {
                    for chunk in missing.chunks(missing.len().div_ceil(fetch_jobs).max(1)) {
                        let fetcher = &fetcher;
                        scope.spawn(move || {
                            for block in chunk {
                                if let Err(error) = fetcher.fetch_case(*block) {
                                    eprintln!("fetch {block}: {error:#}");
                                }
                            }
                        });
                    }
                });
                fetcher.offline = true;
            }
            let campaign = common.campaign()?;
            let replay = MainnetReplay {
                fetcher,
                adapters: campaign.adapters,
                jobs: campaign.jobs,
                timeout: campaign.timeout,
                out: campaign.out,
            };
            let tally = replay.run(&list)?;
            println!("{}", serde_json::to_string_pretty(&tally)?);
        }
        Command::Chain { common, seed, chains, length, reference, minimize_budget } => {
            let campaign = common.campaign()?;
            let driver = ChainDriver {
                adapters: campaign.adapters,
                reference,
                jobs: campaign.jobs,
                timeout: campaign.timeout,
                out: campaign.out,
                seed,
                chains,
                length,
                options: GeneratorOptions::default(),
                minimize_budget,
            };
            let summary = driver.run()?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
    }
    Ok(())
}
