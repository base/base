//! Base snapshot benchmark binary entrypoint.

use base_system_tests::{BenchmarkCli, BenchmarkCommand, RootBenchExit};
use clap::Parser;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    match BenchmarkCli::parse() {
        BenchmarkCli { command:Some(BenchmarkCommand::RootBench(args)) } => {
            let outcome = args.run().await;
            let code = RootBenchExit::code(&outcome);
            if let Err(error) = outcome { eprintln!("{error:#}"); }
            std::process::exit(code);
        }
        cli => cli.run().await,
    }
}
