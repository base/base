//! `diff-fuzz` command-line entry point.

use std::{fs, path::PathBuf, time::Duration};

use base_diff_fuzz::{
    Adapter, AdapterSpec, Campaign, ChainDriver, ChainFollower, ChainFuzzer, ConfigLogs, Era,
    FuzzConfig, Fuzzer, Generator, GeneratorOptions, Lag, MainFuzzConfig, MainFuzzer,
    MainnetFetcher, MainnetReplay, NodeClient, NodeProcess, Request, RpcCampaign, RpcConfig,
    RpcExclude, StateCampaign, StateConfig, StateOp, StateSession,
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
    /// Coverage-guided fuzzing seeded from cached mainnet blocks: real transactions dropped,
    /// reordered, duplicated, spliced, and replayed with edits through deposits, and real
    /// contracts and transactions transplanted into synthetic generator states.
    Mainfuzz {
        #[command(flatten)]
        common: Common,
        /// Mainnet block cache (from `mainnet`).
        #[arg(long, default_value = "mainnet-cache")]
        cache: PathBuf,
        /// How long to run, such as `90s`, `20m`, or `2h`.
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        duration: Duration,
        /// Seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Reference adapter (started with coverage on).
        #[arg(long, default_value = "base")]
        reference: String,
        /// Percentage of fresh cases that are transplants.
        #[arg(long, default_value_t = 35)]
        transplant_percent: u64,
        /// Candidate requests one minimization may run.
        #[arg(long, default_value_t = 150)]
        minimize_budget: usize,
        /// File of coverage feature names of the main corpus (one per line).
        #[arg(long)]
        baseline: Option<PathBuf>,
    },
    /// Prints the request a `mainfuzz` case (such as a finding's `minimized-case.json`) builds.
    BuildCase {
        /// Mainnet block cache.
        #[arg(long, default_value = "mainnet-cache")]
        cache: PathBuf,
        /// Case file.
        case: PathBuf,
    },
    /// Writes the coverage feature names the reference adapter reports for saved fuzz corpus
    /// cases (descriptors) and fresh generator cases, one per line.
    CoverageNames {
        /// Reference adapter as `name=command args...` (started with coverage on).
        #[arg(long)]
        adapter: String,
        /// Fuzz corpus directories of case descriptors.
        #[arg(long)]
        corpus: Vec<PathBuf>,
        /// `mainfuzz` corpus directories (`<out>/corpus`), built from `--cache`.
        #[arg(long)]
        main_corpus: Vec<PathBuf>,
        /// Mainnet block cache for `--main-corpus`.
        #[arg(long, default_value = "mainnet-cache")]
        cache: PathBuf,
        /// Fresh generator cases to add (seed 1).
        #[arg(long, default_value_t = 0)]
        gen_cases: u64,
        /// Worker threads.
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        /// Output file: `name<TAB>cases hitting it<TAB>total hits`, one per line.
        #[arg(long)]
        out: PathBuf,
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
        /// Percentage of chains started just before a fork activation (Isthmus to Denim).
        #[arg(long, default_value_t = 45)]
        crossing_percent: u64,
    },
    /// Coverage-guided chain fuzzing: keeps chain prefixes whose blocks reach new coverage in the
    /// `--coverage-from` adapters and continues them with edits (re-planned blocks, system config
    /// updates, timestamp gaps, forks a few blocks later); writes `chain-coverage.json`.
    ChainFuzz {
        #[command(flatten)]
        common: Common,
        /// Base seed of fresh chains.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// How long to run, such as `90s`, `20m`, or `2h`.
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        duration: Duration,
        /// Reference adapter whose response advances the chain.
        #[arg(long, default_value = "base")]
        reference: String,
        /// Adapters whose coverage guides the run; repeat (default: the reference).
        #[arg(long)]
        coverage_from: Vec<String>,
        /// Only run fresh chains (the baseline); coverage is still measured.
        #[arg(long)]
        unguided: bool,
        /// Percentage of guided runs that start a fresh chain.
        #[arg(long, default_value_t = 20)]
        explore: u64,
        /// Fresh chain length; repeat to pick uniformly.
        #[arg(long = "length", default_values_t = [100, 150, 250, 400, 600, 1000])]
        lengths: Vec<u64>,
        /// Most kept-prefix snapshots held in memory.
        #[arg(long, default_value_t = 800)]
        max_snapshots: usize,
        /// Candidate requests one minimization may run (0 disables minimization).
        #[arg(long, default_value_t = 60)]
        minimize_budget: usize,
        /// Percentage of chains started just before a fork activation (Isthmus to Denim).
        #[arg(long, default_value_t = 75)]
        crossing_percent: u64,
        /// Seconds between timeline samples.
        #[arg(long, default_value_t = 60)]
        sample_secs: u64,
        /// Count only new feature names as new coverage, not new hit-count buckets.
        #[arg(long)]
        no_buckets: bool,
    },
    /// Writes the genesis file that starts real nodes on synthetic chain `seed`.
    NodeGenesis {
        /// Chain seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Output file.
        #[arg(long, default_value = "genesis.json")]
        out: PathBuf,
    },
    /// Differential JSON-RPC fuzzing: every node follows synthetic chain `seed` (started from the
    /// `node-genesis` file) over the Engine API and answers the same generated requests.
    Rpc {
        /// Chain seed (the same as for `node-genesis`).
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Reference adapter that builds the chain, as `name=command args...`.
        #[arg(long)]
        adapter: String,
        /// Node as `name=http_url,auth_url`; repeat for each client. The first is the reference.
        #[arg(long = "node", required = true)]
        nodes: Vec<String>,
        /// Shared Engine API JWT secret (hex).
        #[arg(long)]
        jwt: PathBuf,
        /// Blocks added per round.
        #[arg(long, default_value_t = 8)]
        blocks_per_round: u64,
        /// Chain length after which the chain stops growing.
        #[arg(long, default_value_t = 2000)]
        max_blocks: u64,
        /// Requests per round.
        #[arg(long, default_value_t = 400)]
        requests_per_round: u64,
        /// Worker threads sending requests.
        #[arg(long, default_value_t = 8)]
        jobs: usize,
        /// How long to run, such as `90s`, `20m`, or `2h`.
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        duration: Duration,
        /// Restrict generation to these families; repeat (default: all).
        #[arg(long = "family")]
        families: Vec<String>,
        /// Examples saved per finding signature.
        #[arg(long, default_value_t = 5)]
        examples: usize,
        /// Blocks between safe and head, and between finalized and head.
        #[arg(long, default_value_t = 4)]
        safe_lag: u64,
        /// Blocks between finalized and head.
        #[arg(long, default_value_t = 16)]
        finalized_lag: u64,
        /// Output directory.
        #[arg(long, default_value = "rpc-fuzz-out")]
        out: PathBuf,
        /// Append every request to `<file>.<worker>` before sending it, to find the request a
        /// node crashed on.
        #[arg(long)]
        request_log: Option<PathBuf>,
        /// Leave a node out of matching cases, as `node=method[:body substring]` (a method
        /// ending in `*` is a prefix); repeat. For requests known to crash a node.
        #[arg(long = "exclude")]
        excludes: Vec<String>,
        /// Nodes whose `fuzz_coverage` edges guide the run; repeat for several. Requests to them
        /// are serialized so each reading belongs to one request.
        #[arg(long)]
        coverage_from: Vec<String>,
        /// With `--coverage-from`, only send fresh generator requests (the unguided baseline);
        /// coverage is still measured.
        #[arg(long)]
        unguided: bool,
        /// Percentage of guided requests that are fresh generator requests instead of mutants.
        #[arg(long, default_value_t = 40)]
        explore: u64,
    },
    /// Differential state fuzzing: every node follows synthetic chain `seed` with state-stress
    /// traffic through extensions, reorgs, restarts, and wrong-root blocks, and reads its state
    /// back against the generator's expected state. Nodes are started by `--launch`.
    State {
        /// Chain seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Reference adapter that builds the chain, as `name=command args...`.
        #[arg(long)]
        adapter: String,
        /// Node as `name=http_url,auth_url`; repeat for each client.
        #[arg(long = "node", required = true)]
        nodes: Vec<String>,
        /// Script run as `<launch> <name> <datadir>` that execs node `name` on `datadir`, with
        /// the genesis file and JWT secret in the datadir's parent.
        #[arg(long)]
        launch: PathBuf,
        /// Directory of the datadirs, logs, `genesis.json`, and `jwt.hex`.
        #[arg(long)]
        root: PathBuf,
        /// How long to run, such as `90s`, `20m`, or `2h`.
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        duration: Duration,
        /// Output directory.
        #[arg(long, default_value = "state-fuzz-out")]
        out: PathBuf,
        /// Nodes whose `fuzz_coverage` edges guide the run; repeat for several.
        #[arg(long)]
        coverage_from: Vec<String>,
        /// Only run fresh inputs (the unguided baseline); coverage is still measured.
        #[arg(long)]
        unguided: bool,
        /// Percentage of guided inputs that are fresh instead of mutants.
        #[arg(long, default_value_t = 25)]
        explore: u64,
        /// Canonical height after which the nodes are wiped and the chain starts over.
        #[arg(long, default_value_t = 400)]
        max_blocks: u64,
        /// Deepest reorg.
        #[arg(long, default_value_t = 40)]
        max_depth: u64,
        /// Blocks between safe and head.
        #[arg(long, default_value_t = 4)]
        safe_lag: u64,
        /// Blocks between finalized and head (reorgs never go below finalized).
        #[arg(long, default_value_t = 64)]
        finalized_lag: u64,
        /// Replays one minimization may run (0 disables minimization).
        #[arg(long, default_value_t = 24)]
        minimize_budget: usize,
        /// Instead of fuzzing, replay the operations in this file (`ops.json` of a reproducer)
        /// from genesis and print the divergences, minimizing them within `--minimize-budget`.
        #[arg(long)]
        replay: Option<PathBuf>,
    },
}

/// Eras of the chains real nodes follow: the Engine API payloads they take start at Isthmus.
fn node_chain_options() -> GeneratorOptions {
    GeneratorOptions {
        eras: vec![Era::Isthmus, Era::Jovian, Era::Azul, Era::Beryl, Era::Cobalt, Era::Denim],
        max_nonce_deposit_creates: false,
        ..GeneratorOptions::default()
    }
}

/// Options of the chains the state campaign drives: the node eras, with state-stress traffic.
fn state_chain_options() -> GeneratorOptions {
    GeneratorOptions { state_stress: true, ..node_chain_options() }
}

/// Generator options of synthetic chains: `crossing_percent` of them start just before an
/// upgrade activation. Like real-node chains, they leave out deposit CREATEs from the
/// `u64::MAX`-nonce key.
fn chain_options(crossing_percent: u64) -> GeneratorOptions {
    GeneratorOptions {
        activation_percent: crossing_percent.div_ceil(3),
        max_nonce_deposit_creates: false,
        ..GeneratorOptions::default()
    }
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
        Command::NodeGenesis { seed, out } => {
            ChainFollower::write_genesis(seed, node_chain_options(), &out, false)?;
        }
        Command::Rpc {
            seed,
            adapter,
            nodes,
            jwt,
            blocks_per_round,
            max_blocks,
            requests_per_round,
            jobs,
            duration,
            families,
            examples,
            safe_lag,
            finalized_lag,
            out,
            request_log,
            excludes,
            coverage_from,
            unguided,
            explore,
        } => {
            fs::create_dir_all(&out)?;
            let _ = fs::remove_file(out.join("payloads.jsonl"));
            let nodes = nodes
                .iter()
                .map(|n| NodeClient::parse(n, &jwt))
                .collect::<eyre::Result<Vec<_>>>()?;
            let adapter = Adapter::new(AdapterSpec::parse(&adapter)?, Duration::from_secs(120));
            let lag = Lag { safe: safe_lag, finalized: finalized_lag };
            let mut follower = ChainFollower::start(
                seed,
                node_chain_options(),
                adapter,
                nodes,
                lag,
                Some(out.join("payloads.jsonl")),
            )?;
            let config = RpcConfig {
                seed,
                blocks_per_round,
                max_blocks,
                requests_per_round,
                jobs,
                duration,
                families,
                examples,
                out,
                request_log,
                excludes: excludes
                    .iter()
                    .map(|e| RpcExclude::parse(e))
                    .collect::<eyre::Result<_>>()?,
                coverage_from,
                guided: !unguided,
                explore_percent: explore,
            };
            let summary = RpcCampaign::run(&mut follower, &config)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::State {
            seed,
            adapter,
            nodes,
            launch,
            root,
            duration,
            out,
            coverage_from,
            unguided,
            explore,
            max_blocks,
            max_depth,
            safe_lag,
            finalized_lag,
            minimize_budget,
            replay,
        } => {
            fs::create_dir_all(&root)?;
            fs::create_dir_all(&out)?;
            let jwt = root.join("jwt.hex");
            if !jwt.exists() {
                fs::write(&jwt, alloy_primitives::hex::encode(alloy_primitives::B256::random()))?;
            }
            let nodes = nodes
                .iter()
                .map(|n| NodeClient::parse(n, &jwt))
                .collect::<eyre::Result<Vec<_>>>()?;
            let procs = nodes
                .iter()
                .map(|n| NodeProcess::new(n.name.clone(), launch.clone(), root.clone()))
                .collect();
            let adapter = Adapter::new(AdapterSpec::parse(&adapter)?, Duration::from_secs(300));
            let lag = Lag { safe: safe_lag, finalized: finalized_lag };
            let mut session = StateSession::start(
                seed,
                state_chain_options(),
                adapter,
                nodes,
                procs,
                lag,
                max_depth.min(finalized_lag),
            )?;
            if let Some(path) = replay {
                let ops: Vec<StateOp> = serde_json::from_str(&fs::read_to_string(&path)?)
                    .wrap_err_with(|| format!("parsing {}", path.display()))?;
                let mut divergences = session.replay(&ops)?;
                session.save_reproducer(&out.join("replay"), &divergences)?;
                if let Some(first) = divergences.first().filter(|_| minimize_budget > 0) {
                    let signature = first.signature();
                    let ops =
                        StateCampaign::minimize(&mut session, &ops, &signature, minimize_budget)?;
                    divergences = session.replay(&ops)?;
                    session.save_reproducer(&out.join("minimized"), &divergences)?;
                }
                println!("{}", serde_json::to_string_pretty(&divergences)?);
                return Ok(());
            }
            let config = StateConfig {
                seed,
                duration,
                out,
                coverage_from,
                guided: !unguided,
                explore_percent: explore,
                max_blocks,
                minimize_budget,
            };
            let summary = StateCampaign::run(&mut session, &config)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
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
        Command::Mainfuzz {
            common,
            cache,
            duration,
            seed,
            reference,
            transplant_percent,
            minimize_budget,
            baseline,
        } => {
            let campaign = common.campaign()?;
            let baseline = match baseline {
                Some(path) => fs::read_to_string(path)?
                    .lines()
                    .map(|line| line.split('\t').next().unwrap_or_default().to_owned())
                    .collect(),
                None => Default::default(),
            };
            let fuzzer = MainFuzzer {
                config: MainFuzzConfig {
                    adapters: campaign.adapters,
                    reference,
                    jobs: campaign.jobs,
                    timeout: campaign.timeout,
                    out: campaign.out,
                    duration,
                    seed,
                    transplant_percent,
                    minimize_budget,
                    baseline,
                },
                pool: std::sync::Arc::new(base_diff_fuzz::BlockPool::open(cache)?),
            };
            let stats = fuzzer.run()?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::BuildCase { cache, case } => {
            let case: base_diff_fuzz::MainCase = serde_json::from_slice(&fs::read(case)?)?;
            let request = case.build(&base_diff_fuzz::BlockPool::open(cache)?)?;
            println!("{}", serde_json::to_string(&request)?);
        }
        Command::CoverageNames { adapter, corpus, main_corpus, cache, gen_cases, jobs, out } => {
            let mut spec = AdapterSpec::parse(&adapter)?;
            spec.command
                .splice(0..0, ["env".to_owned(), format!("{}=1", base_diff_fuzz::COVERAGE_ENV)]);
            let options = GeneratorOptions::default();
            let read_dir = |dir: &PathBuf| -> eyre::Result<Vec<Vec<u8>>> {
                let mut files = Vec::new();
                for entry in fs::read_dir(dir)? {
                    let path = entry?.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        files.push(fs::read(&path)?);
                    }
                }
                Ok(files)
            };
            let mut cases: Vec<base_diff_fuzz::FuzzCase> = Vec::new();
            for dir in &corpus {
                for file in read_dir(dir)? {
                    cases.push(serde_json::from_slice(&file)?);
                }
            }
            let mut main_cases: Vec<base_diff_fuzz::MainCase> = Vec::new();
            for dir in &main_corpus {
                for file in read_dir(dir)? {
                    main_cases.push(serde_json::from_slice(&file)?);
                }
            }
            let pool = if main_cases.is_empty() {
                None
            } else {
                Some(base_diff_fuzz::BlockPool::open(cache)?)
            };
            let generator = Generator::new(1, options.clone());
            let (fuzz, main) = (cases.len() as u64, main_cases.len() as u64);
            let total = fuzz + main + gen_cases;
            let next = std::sync::atomic::AtomicU64::new(0);
            let names =
                std::sync::Mutex::new(std::collections::BTreeMap::<String, (u64, u64)>::new());
            std::thread::scope(|scope| {
                for _ in 0..jobs.max(1) {
                    scope.spawn(|| {
                        let mut adapter =
                            base_diff_fuzz::Adapter::new(spec.clone(), Duration::from_secs(120));
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if i >= total {
                                break;
                            }
                            let request = if i < fuzz {
                                cases[i as usize].build(&options).request
                            } else if i < fuzz + main {
                                let pool = pool.as_ref().expect("main cases need the cache");
                                match main_cases[(i - fuzz) as usize].build(pool) {
                                    Ok(request) => request,
                                    Err(_) => continue,
                                }
                            } else {
                                generator.case(i - fuzz - main)
                            };
                            if let Ok(response) = adapter.call(&request)
                                && let Some(coverage) = response.coverage
                            {
                                let mut names = names.lock().unwrap();
                                for (name, count) in coverage {
                                    let entry = names.entry(name).or_default();
                                    entry.0 += 1;
                                    entry.1 += count;
                                }
                            }
                        }
                    });
                }
            });
            let names = names.into_inner().unwrap();
            let lines: Vec<String> = names
                .iter()
                .map(|(name, (cases, hits))| format!("{name}\t{cases}\t{hits}"))
                .collect();
            fs::write(&out, lines.join("\n"))?;
            println!("{} names from {total} cases", names.len());
        }
        Command::Chain {
            common,
            seed,
            chains,
            length,
            reference,
            minimize_budget,
            crossing_percent,
        } => {
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
                options: chain_options(crossing_percent),
                minimize_budget,
            };
            let summary = driver.run()?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::ChainFuzz {
            common,
            seed,
            duration,
            reference,
            coverage_from,
            unguided,
            explore,
            lengths,
            max_snapshots,
            minimize_budget,
            crossing_percent,
            sample_secs,
            no_buckets,
        } => {
            let campaign = common.campaign()?;
            let fuzzer = ChainFuzzer {
                coverage_from: if coverage_from.is_empty() {
                    vec![reference.clone()]
                } else {
                    coverage_from
                },
                driver: ChainDriver {
                    adapters: campaign.adapters,
                    reference,
                    jobs: campaign.jobs,
                    timeout: campaign.timeout,
                    out: campaign.out,
                    seed,
                    chains: 0,
                    length: 0,
                    options: chain_options(crossing_percent),
                    minimize_budget,
                },
                duration,
                guided: !unguided,
                explore_percent: explore,
                lengths,
                max_snapshots,
                sample_every: Duration::from_secs(sample_secs),
                buckets: !no_buckets,
            };
            let stats = fuzzer.run()?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
    }
    Ok(())
}
