//! `base-tiger-chaos`: records chains with base-reth and replays them into tiger under chaos.

use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc, time::Instant};

use base_tiger_chaos::{ChaosConfig, ChaosRun, Recording};
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(about = "Chaos and Jepsen-style history checking of tiger's database")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Records a chain: base-reth follows the state generator's chain of branches; writes the
    /// recording directory.
    Record {
        /// Chain seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Canonical height to stop at.
        #[arg(long, default_value_t = 300)]
        blocks: u64,
        /// Reference adapter that builds the chain, as `name=command args...`.
        #[arg(long)]
        adapter: String,
        /// base-reth node as `base=http_url,auth_url`.
        #[arg(long)]
        node: String,
        /// Launch script.
        #[arg(long)]
        launch: PathBuf,
        /// Scratch directory of the node.
        #[arg(long)]
        root: PathBuf,
        /// Recording directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Replays recordings into tiger under chaos, run after run, until `--runs` or `--hours`.
    Chaos {
        /// Recording directories, used in turn.
        #[arg(long = "recording", required = true)]
        recordings: Vec<PathBuf>,
        /// Directory of the runs.
        #[arg(long)]
        root: PathBuf,
        /// Launch script.
        #[arg(long)]
        launch: PathBuf,
        /// tiger binary.
        #[arg(long)]
        tiger_bin: PathBuf,
        /// base-reth binary.
        #[arg(long)]
        base_bin: PathBuf,
        /// Port offset.
        #[arg(long, default_value_t = 200)]
        port_offset: u16,
        /// Reader threads.
        #[arg(long, default_value_t = 6)]
        readers: usize,
        /// Campaign seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// First run index.
        #[arg(long, default_value_t = 0)]
        first_run: u64,
        /// Runs (0: until `--hours`).
        #[arg(long, default_value_t = 0)]
        runs: u64,
        /// Hours to run.
        #[arg(long, default_value_t = 1.0)]
        hours: f64,
        /// Chaos actions per 1000 steps.
        #[arg(long, default_value_t = 40)]
        intensity: u64,
        /// Comma-separated fault modes tiger supports.
        #[arg(long, default_value = "torn,lose_unsynced,enospc")]
        fault_modes: String,
        /// Percentage of runs starting from base-reth's snapshot datadir.
        #[arg(long, default_value_t = 30)]
        snapshot_percent: u64,
        /// Percentage of crashes base-reth reopens first.
        #[arg(long, default_value_t = 20)]
        crash_handoff_percent: u64,
        /// Summary file, rewritten after every run.
        #[arg(long)]
        summary: PathBuf,
    },
}

fn main() -> eyre::Result<()> {
    match Cli::parse().command {
        Command::Record { seed, blocks, adapter, node, launch, root, out } => {
            let recording = Recording::record(seed, blocks, &adapter, &launch, &root, &node, &out)?;
            println!(
                "{}",
                json!({ "steps": recording.steps.len(), "blocks": recording.blocks.len(), "height": recording.oracle.len() })
            );
        }
        Command::Chaos {
            recordings,
            root,
            launch,
            tiger_bin,
            base_bin,
            port_offset,
            readers,
            seed,
            first_run,
            runs,
            hours,
            intensity,
            fault_modes,
            snapshot_percent,
            crash_handoff_percent,
            summary,
        } => {
            fs::create_dir_all(&root)?;
            let loaded: Vec<Arc<Recording>> = recordings
                .iter()
                .map(|r| Recording::load(r).map(Arc::new))
                .collect::<eyre::Result<_>>()?;
            let started = Instant::now();
            let mut totals: BTreeMap<String, u64> = BTreeMap::new();
            let mut reads: BTreeMap<String, u64> = BTreeMap::new();
            let mut violations = Vec::new();
            let mut done = 0u64;
            let mut run = first_run;
            loop {
                if (runs > 0 && done >= runs)
                    || (runs == 0 && started.elapsed().as_secs_f64() > hours * 3600.0)
                {
                    break;
                }
                let which = (run as usize) % loaded.len();
                let config = ChaosConfig {
                    recording: recordings[which].clone(),
                    root: root.clone(),
                    launch: launch.clone(),
                    tiger_bin: tiger_bin.clone(),
                    base_bin: base_bin.clone(),
                    port_offset,
                    readers,
                    seed,
                    intensity,
                    fault_modes: fault_modes.split(',').map(str::to_owned).collect(),
                    snapshot_percent,
                    crash_handoff_percent,
                };
                let report = ChaosRun::execute(&config, &loaded[which], run)?;
                eprintln!(
                    "run {run}: {} steps, {} violations, events {:?}, {:.0}s",
                    report.steps,
                    report.violations.len(),
                    report.events,
                    report.seconds
                );
                for (k, v) in &report.events {
                    *totals.entry(k.clone()).or_default() += v;
                }
                for (k, v) in &report.reads {
                    *reads.entry(k.clone()).or_default() += v;
                }
                if !report.violations.is_empty() {
                    violations.push(json!({ "run": run, "seed": report.seed, "dir": root.join(format!("run-{run}")), "violations": report.violations }));
                } else {
                    // Keep failing runs' datadirs and logs; drop passing ones.
                    let dir = root.join(format!("run-{run}"));
                    let _ = fs::remove_dir_all(dir.join("tiger"));
                }
                done += 1;
                run += 1;
                let all_reads: u64 = reads.values().sum();
                fs::write(
                    &summary,
                    serde_json::to_vec_pretty(&json!({
                        "runs": done,
                        "hours": started.elapsed().as_secs_f64() / 3600.0,
                        "events": totals,
                        "reads": reads,
                        "totalReads": all_reads,
                        "failingRuns": violations.len(),
                        "violations": violations,
                    }))?,
                )?;
            }
        }
    }
    Ok(())
}
