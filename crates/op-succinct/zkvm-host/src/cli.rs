use clap::Parser;
use zkvm_common::BootInfoWithoutRollupConfig;
use alloy_primitives::B256;
use std::str::FromStr;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct SP1KonaCliArgs {
    #[arg(long)]
    l1_head: String,

    #[arg(long)]
    l2_output_root: String,

    #[arg(long)]
    l2_claim: String,

    #[arg(long)]
    l2_claim_block: u64,

    #[arg(long)]
    chain_id: u64,
}

impl From<SP1KonaCliArgs> for BootInfoWithoutRollupConfig {
    fn from(args: SP1KonaCliArgs) -> Self {
        BootInfoWithoutRollupConfig {
            l1_head: B256::from_str(&args.l1_head).unwrap(),
            l2_output_root: B256::from_str(&args.l2_output_root).unwrap(),
            l2_claim: B256::from_str(&args.l2_claim).unwrap(),
            l2_claim_block: args.l2_claim_block,
            chain_id: args.chain_id,
        }
    }
}
