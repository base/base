use std::env;

use alloy_primitives::Address;
use alloy_provider::ProviderBuilder;
use alloy_transport_http::reqwest::Url;
use anyhow::Result;
use clap::Parser;
use fault_proof::{
    challenger::OPSuccinctChallenger, contract::DisputeGameFactory, prometheus::ChallengerGauge,
};
use op_succinct_host_utils::{
    metrics::{init_metrics, MetricsGauge},
    setup_logger,
};
use op_succinct_signer_utils::Signer;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = ".env.challenger")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logger();

    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    let challenger_signer = Signer::from_env()?;

    let l1_provider = ProviderBuilder::default()
        .connect_http(env::var("L1_RPC").unwrap().parse::<Url>().unwrap());

    let factory = DisputeGameFactory::new(
        env::var("FACTORY_ADDRESS")
            .expect("FACTORY_ADDRESS must be set")
            .parse::<Address>()
            .unwrap(),
        l1_provider.clone(),
    );

    let mut challenger =
        OPSuccinctChallenger::from_env(l1_provider, factory, challenger_signer).await.unwrap();

    // Initialize challenger gauges.
    ChallengerGauge::register_all();

    // Initialize metrics exporter.
    init_metrics(&challenger.config.metrics_port);

    // Initialize the metrics gauges.
    ChallengerGauge::init_all();

    challenger.run().await.expect("Runs in an infinite loop");

    Ok(())
}
