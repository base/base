//! Devnet contract deployment helpers for benchmark setup.

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use base_test_utils::{FreeTransferERC20, MockUniswapV3Router};
use tracing::info;

use crate::error::BenchmarkError;

/// Addresses of a deployed mock Uniswap V3 setup on a devnet.
#[derive(Debug, Clone)]
pub struct UniswapV3Addresses {
    /// Mock router address.
    pub router: Address,
    /// First token address (used as `token_in`).
    pub token_in: Address,
    /// Second token address (used as `token_out`).
    pub token_out: Address,
}

/// Deploy a mock Uniswap V3 setup on a devnet node.
///
/// Deploys two [`FreeTransferERC20`] tokens and a [`MockUniswapV3Router`], then
/// pre-mints 1 billion tokens of each type into the router so swaps never run
/// dry during a benchmark run.
pub async fn deploy_uniswap_v3(
    rpc_url: &str,
    private_key: &str,
) -> Result<UniswapV3Addresses, BenchmarkError> {
    let signer: PrivateKeySigner = private_key
        .trim_start_matches("0x")
        .parse()
        .map_err(|e| BenchmarkError::Config(format!("invalid deploy key: {e}")))?;
    let wallet = EthereumWallet::new(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(
            rpc_url
                .parse()
                .map_err(|e| BenchmarkError::Config(format!("invalid rpc url: {e}")))?,
        );

    info!("deploying mock Uniswap V3 tokens and router");

    let token_in = FreeTransferERC20::deploy(&provider, "Bench Token A".into(), "BTKA".into(), 18)
        .await
        .map_err(|e| BenchmarkError::Config(format!("token_in deploy failed: {e}")))?;
    let token_in_addr = *token_in.address();
    info!(address = %token_in_addr, "deployed token_in");

    let token_out =
        FreeTransferERC20::deploy(&provider, "Bench Token B".into(), "BTKB".into(), 18)
            .await
            .map_err(|e| BenchmarkError::Config(format!("token_out deploy failed: {e}")))?;
    let token_out_addr = *token_out.address();
    info!(address = %token_out_addr, "deployed token_out");

    let router = MockUniswapV3Router::deploy(&provider)
        .await
        .map_err(|e| BenchmarkError::Config(format!("router deploy failed: {e}")))?;
    let router_addr = *router.address();
    info!(address = %router_addr, "deployed mock Uniswap V3 router");

    // Pre-fund the router with 1 billion tokens of each type so it always has
    // sufficient output-token reserves during the benchmark.
    let large_amount = U256::from(1_000_000_000u64) * U256::from(10u64).pow(U256::from(18u64));

    token_in
        .mint(router_addr, large_amount)
        .send()
        .await
        .map_err(|e| BenchmarkError::Config(format!("token_in mint failed: {e}")))?
        .get_receipt()
        .await
        .map_err(|e| BenchmarkError::Config(format!("token_in mint receipt failed: {e}")))?;

    token_out
        .mint(router_addr, large_amount)
        .send()
        .await
        .map_err(|e| BenchmarkError::Config(format!("token_out mint failed: {e}")))?
        .get_receipt()
        .await
        .map_err(|e| BenchmarkError::Config(format!("token_out mint receipt failed: {e}")))?;

    info!(
        router = %router_addr,
        token_in = %token_in_addr,
        token_out = %token_out_addr,
        "mock Uniswap V3 setup complete"
    );

    Ok(UniswapV3Addresses { router: router_addr, token_in: token_in_addr, token_out: token_out_addr })
}
