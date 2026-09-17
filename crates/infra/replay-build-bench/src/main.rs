//! Minimal glue for the replayed-building benchmark binary.

use base_replay_build_bench::ReplayBuildBench;
use clap::Parser;

fn main() -> eyre::Result<()> {
    ReplayBuildBench::parse().execute()
}
