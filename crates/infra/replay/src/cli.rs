use std::{
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use eyre::Result;

use crate::{BenchmarkReport, Capture, Fixture, HttpRpc};

/// Capture mainnet workloads and benchmark production components offline.
#[derive(Debug, Parser)]
#[command(name = "base-replay")]
pub struct Cli {
    /// Operation to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Online capture and strictly offline validation/benchmark operations.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture a pinned block selection and optional parent-state proofs.
    Capture {
        /// Read-only Base mainnet RPC endpoint; never persisted in the fixture.
        #[arg(long, env = "BASE_REPLAY_RPC_URL", default_value = "https://mainnet.base.org")]
        rpc_url: String,
        /// Selection and state accesses.
        #[command(flatten)]
        selection: Capture,
        /// New fixture path. Existing files are not overwritten.
        #[arg(long)]
        out: PathBuf,
    },
    /// Validate all recorded identities, proof values and roots offline.
    Verify {
        /// Fixture JSON file.
        fixture: PathBuf,
    },
    /// Benchmark transaction preparation, Fjord pricing and proof tries without network I/O.
    Bench {
        /// Fixture JSON file.
        fixture: PathBuf,
        /// Number of measured batches.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(3..))]
        samples: u32,
        /// Iterations per batch; raise for short workloads.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
        iterations: u32,
    },
}

impl Cli {
    /// Runs the selected command. Benchmark JSON goes to stdout for archiving/comparison.
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Capture { rpc_url, selection, out } => {
                let mut rpc = HttpRpc::new(&rpc_url)?;
                let mut fixture = selection.capture(&mut rpc).await?;
                fixture.rpc.exchanges = rpc.exchanges;
                let mut output = BufWriter::new(File::create_new(out)?);
                serde_json::to_writer_pretty(&mut output, &fixture)?;
                output.flush()?;
                eprintln!(
                    "captured block {}: {} transactions, {} account proofs",
                    fixture.header.number,
                    fixture.transactions.len(),
                    fixture.proofs.len()
                );
            }
            Command::Verify { fixture } => {
                let fixture: Fixture =
                    serde_json::from_reader(BufReader::new(File::open(fixture)?))?;
                fixture.validate()?;
                println!(
                    "verified block {}: {} transactions, {} account proofs",
                    fixture.header.number,
                    fixture.transactions.len(),
                    fixture.proofs.len()
                );
            }
            Command::Bench { fixture, samples, iterations } => {
                let fixture = serde_json::from_reader(BufReader::new(File::open(fixture)?))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&BenchmarkReport::run(
                        &fixture, samples, iterations
                    )?)?
                );
            }
        }
        Ok(())
    }
}
