use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

use crate::{
    BatchWorkload, BenchmarkReport, Capture, CaptureOracle, Fixture, HttpRpc, Measurement,
};

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
        /// Capture a full stateless execution witness. Requires every block transaction.
        #[arg(long)]
        execution: bool,
        /// Omit the duplicated JSON-RPC response tape from the exported fixture.
        #[arg(long)]
        compact: bool,
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
    /// Round-trip consecutive fixtures through batching, blobs, channels and batch derivation.
    BenchBatch {
        /// One or more consecutive block fixtures, in canonical order.
        #[arg(required = true)]
        fixtures: Vec<PathBuf>,
        /// Number of measured batches.
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u32).range(3..))]
        samples: u32,
        /// Iterations per sample.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
        iterations: u32,
    },
}

impl Cli {
    /// Runs the selected command. Benchmark JSON goes to stdout for archiving/comparison.
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Capture { rpc_url, selection, execution, compact, out } => {
                let mut rpc = HttpRpc::new(&rpc_url)?;
                let mut fixture = selection.capture(&mut rpc).await?;
                if execution {
                    fixture.execution_payload()?;
                    eprintln!(
                        "capturing stateless witness for block {}: {} transactions",
                        fixture.header.number,
                        fixture.transactions.len()
                    );
                    fixture.execution =
                        Some(CaptureOracle::capture(fixture.clone(), rpc_url).await?);
                }
                if !compact {
                    fixture.rpc.exchanges = rpc.exchanges;
                }
                fixture.write(out)?;
                eprintln!(
                    "captured block {}: {} transactions, {} account proofs",
                    fixture.header.number,
                    fixture.transactions.len(),
                    fixture.proofs.len()
                );
            }
            Command::Verify { fixture } => {
                let fixture = Fixture::read(fixture)?;
                fixture.validate()?;
                println!(
                    "verified block {}: {} transactions, {} account proofs",
                    fixture.header.number,
                    fixture.transactions.len(),
                    fixture.proofs.len()
                );
                if let Some(witness) = &fixture.execution {
                    println!(
                        "full mainnet header matched: {} trie nodes, {} bytecodes, {} gas",
                        witness.nodes.len(),
                        witness.codes.len(),
                        fixture.header.gas_used
                    );
                }
            }
            Command::Bench { fixture, samples, iterations } => {
                let fixture = Fixture::read(fixture)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&BenchmarkReport::run(
                        &fixture, samples, iterations
                    )?)?
                );
            }
            Command::BenchBatch { fixtures, samples, iterations } => {
                let fixtures =
                    fixtures.iter().map(Fixture::read).collect::<Result<Vec<Fixture>>>()?;
                let workload = BatchWorkload::new(&fixtures, Default::default())?;
                let encoded = workload.encode()?;
                workload.verify(&workload.decode(&encoded)?)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "fixture_hashes": fixtures.iter().map(|f| serde_json::to_vec(f)
                            .map(alloy_primitives::keccak256)).collect::<Result<Vec<_>, _>>()?,
                        "blocks": fixtures.len(), "transactions": workload.user_transactions,
                        "user_bytes": workload.user_bytes, "channel_bytes": encoded.channel_bytes,
                        "frames": encoded.frames, "synthetic_l1_info": workload.synthetic_l1_info,
                        "iterations": iterations, "debug_assertions": cfg!(debug_assertions),
                        "arch": std::env::consts::ARCH,
                        "measurements": [
                            Measurement::measure("batch-encode", samples, iterations, || workload.encode())?,
                            Measurement::measure("batch-derive", samples, iterations, || workload.decode(&encoded))?,
                        ],
                    }))?
                );
            }
        }
        Ok(())
    }
}
