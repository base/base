#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

//! Flashblocks E2E - End-to-end regression testing for node-reth flashblocks RPC.

mod cli;

use alloy_primitives::Address;
use base_testing_flashblocks_e2e::{
    TestClient, build_test_suite, list_tests, print_results_json, print_results_text, run_tests,
};
use clap::Parser;
use cli::{Args, OutputFormat};
use eyre::{Result, WrapErr};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (ignores errors if file doesn't exist)
    let _ = dotenvy::dotenv();

    let args = Args::parse();

    // Initialize tracing
    cli::init_tracing(args.verbose);

    // Get private key from environment (optional)
    let private_key = std::env::var("PRIVATE_KEY").ok();

    if private_key.is_some() {
        tracing::info!("Private key loaded from environment");
    } else {
        tracing::warn!("No PRIVATE_KEY set - tests requiring transaction signing will be skipped");
    }

    // Parse recipient address if provided
    let recipient: Option<Address> = args
        .recipient
        .as_ref()
        .map(|s| s.parse())
        .transpose()
        .wrap_err("Invalid recipient address")?;

    if recipient.is_none() {
        tracing::warn!("No --recipient set - tests requiring ETH transfers will be skipped");
    }

    // Create test client (chain ID is fetched from RPC)
    let client =
        TestClient::new(&args.rpc_url, &args.flashblocks_ws_url, private_key.as_deref(), recipient)
            .await?;

    if let Some(addr) = client.signer_address() {
        tracing::info!(address = ?addr, "Signer configured");
    }
    if let Some(addr) = client.recipient() {
        tracing::info!(address = ?addr, "Recipient configured");
    }

    // Build test suite
    let suite = build_test_suite();

    if args.list {
        list_tests(&suite);
        return Ok(());
    }

    // Run tests
    let results = run_tests(&client, &suite, args.filter.as_deref(), args.keep_going).await;

    // Output results
    match args.format {
        OutputFormat::Text => print_results_text(&results),
        OutputFormat::Json => print_results_json(&results)?,
    }

    // Exit with error code if any tests failed
    if results.iter().any(|r| !r.passed) {
        std::process::exit(1);
    }

    Ok(())
}
