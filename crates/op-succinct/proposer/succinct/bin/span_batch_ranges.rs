use anyhow::Result;
use clap::Parser;
use host_utils::fetcher::{ChainMode, OPSuccinctDataFetcher};
use kona_primitives::RollupConfig;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Parser)]
struct Args {
    #[clap(long)]
    start: u64,
    #[clap(long)]
    end: u64,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct SpanBatchRequest {
    startBlock: u64,
    endBlock: u64,
    l2ChainId: u64,
    l2Node: String,
    l1Rpc: String,
    l1Beacon: String,
    batchSender: String,
}

#[derive(Deserialize, Debug, Clone)]
struct SpanBatchResponse {
    ranges: Vec<SpanBatchRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpanBatchRange {
    start: u64,
    end: u64,
}

async fn get_span_batch_ranges_from_server(
    data_fetcher: &OPSuccinctDataFetcher,
    start: u64,
    end: u64,
    l2_chain_id: u64,
    batch_sender: &str,
) -> Result<Vec<SpanBatchRange>> {
    let client = Client::new();
    let request = SpanBatchRequest {
        startBlock: start,
        endBlock: end,
        l2ChainId: l2_chain_id,
        l2Node: data_fetcher.l2_node_rpc.clone(),
        l1Rpc: data_fetcher.l1_rpc.clone(),
        l1Beacon: data_fetcher.l1_beacon_rpc.clone(),
        batchSender: batch_sender.to_string(),
    };

    let span_batch_server_url =
        env::var("SPAN_BATCH_SERVER_URL").unwrap_or("http://localhost:8080".to_string());
    let query_url = format!("{}/span-batch-ranges", span_batch_server_url);

    let response: SpanBatchResponse =
        client.post(&query_url).json(&request).send().await?.json().await?;

    Ok(response.ranges)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let data_fetcher = OPSuccinctDataFetcher::new();
    let l2_chain_id = data_fetcher.get_chain_id(ChainMode::L2).await?;
    let rollup_config = RollupConfig::from_l2_chain_id(l2_chain_id).unwrap();

    let span_batch_ranges = get_span_batch_ranges_from_server(
        &data_fetcher,
        args.start,
        args.end,
        l2_chain_id,
        rollup_config.genesis.system_config.clone().unwrap().batcher_address.to_string().as_str(),
    )
    .await?;

    println!("Span batch ranges: {:?}", span_batch_ranges);

    Ok(())
}
