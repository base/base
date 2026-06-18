//! Checks TDX prover RPC egress to zeronet endpoints.

use std::{process::ExitCode, time::Duration};

const RPC_URLS: &[&str] = &[
    "https://c3-chainproxy-eth-hoodi-full-dev.cbhq.net",
    "https://base-zeronet-reth-proofs-donotuse.cbhq.net:8545",
];

#[tokio::main]
async fn main() -> ExitCode {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("failed to build HTTP client: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut failed = false;
    for url in RPC_URLS {
        match block_number(&client, url).await {
            Ok(body) => println!("{url}: {body}"),
            Err(err) => {
                failed = true;
                eprintln!("{url}: {err}");
            }
        }
    }

    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

async fn block_number(client: &reqwest::Client, url: &str) -> Result<String, reqwest::Error> {
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#)
        .send()
        .await?
        .error_for_status()?;

    response.text().await
}
