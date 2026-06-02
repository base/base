//! Local system benchmark for B-20 precompile ZK proving cycles.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p base-system-tests --bench b20_zk_proving
//! ```
//!
//! Requires local L2, rollup, and ZK prover RPC endpoints with `SP1_PROVER=dry-run` mode.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolInterface};
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20TokenRole, IActivationRegistry, IB20,
};
use base_system_tests::{ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, B20PrecompileClient};
use clap::Parser;
use eyre::{Result, WrapErr};
use tokio::runtime::Runtime;
use url::Url;

pub mod common;

use common::{
    BenchDisplay, BenchProvider, CycleReport, OperationReport, ZkProofBench, ZkProofBenchConfig,
};

const WORKLOAD_TXS: u64 = 10;
const INITIAL_SUPPLY: u64 = 1_000_000;
const TRANSFER_AMOUNT: u64 = 100;
const TRANSFER_WITH_MEMO_AMOUNT: u64 = 101;
const TRANSFER_FROM_AMOUNT: u64 = 102;
const TRANSFER_FROM_WITH_MEMO_AMOUNT: u64 = 103;
const ALLOWANCE_AMOUNT: u64 = 1_000;
const UPDATED_SUPPLY_CAP: u64 = 2_000_000;
const HEAVY_CONTRACT_URI: &str =
    "ipfs://b20-zk-bench/metadata/heavy-interaction/with/a/longer/contract-uri/payload";

/// Local B-20 ZK dry-run proving benchmark.
#[derive(Debug)]
pub struct B20ZkProvingBench;

fn main() -> Result<()> {
    Runtime::new()
        .wrap_err("failed to start tokio runtime")?
        .block_on(B20ZkProvingBench::run(B20ZkProvingConfig::parse()))
}

/// CLI configuration for the local B-20 ZK proving benchmark.
#[derive(Clone, Debug, Parser)]
pub struct B20ZkProvingConfig {
    /// Cargo passes this flag to custom benchmark binaries.
    #[arg(long = "bench", hide = true)]
    pub cargo_bench: bool,
    /// L2 execution RPC URL.
    #[arg(long, default_value = "http://localhost:8645")]
    pub l2_rpc_url: Url,
    /// Rollup RPC URL.
    #[arg(long, default_value = "http://localhost:8649")]
    pub rollup_rpc_url: Url,
    /// ZK prover RPC URL.
    #[arg(long, default_value = "http://localhost:9000")]
    pub zk_prover_url: Url,
    /// Local benchmark L2 chain ID.
    #[arg(long, default_value_t = 84538453)]
    pub l2_chain_id: u64,
    /// Polling interval in milliseconds for block, receipt, and account funding waits.
    #[arg(long = "block-poll-interval-ms", default_value = "500", value_parser = parse_duration_millis)]
    pub block_poll_interval: Duration,
    /// Transaction receipt timeout in seconds.
    #[arg(long = "tx-receipt-timeout-secs", default_value = "60", value_parser = parse_duration_secs)]
    pub tx_receipt_timeout: Duration,
    /// Account funding timeout in seconds.
    #[arg(long = "account-funding-timeout-secs", default_value = "15", value_parser = parse_duration_secs)]
    pub account_funding_timeout: Duration,
    /// Proof status polling interval in seconds.
    #[arg(long = "proof-poll-interval-secs", default_value = "5", value_parser = parse_duration_secs)]
    pub proof_poll_interval: Duration,
    /// Proof job timeout in seconds.
    #[arg(long = "proof-timeout-secs", default_value = "900", value_parser = parse_duration_secs)]
    pub proof_timeout: Duration,
    /// Timeout in seconds for waiting until workload blocks are safe.
    #[arg(long = "safe-l2-timeout-secs", default_value = "300", value_parser = parse_duration_secs)]
    pub safe_l2_timeout: Duration,
}

fn parse_duration_millis(value: &str) -> Result<Duration, std::num::ParseIntError> {
    value.parse().map(Duration::from_millis)
}

fn parse_duration_secs(value: &str) -> Result<Duration, std::num::ParseIntError> {
    value.parse().map(Duration::from_secs)
}

/// Sends the fixed B-20 call sequence used by the benchmark.
#[derive(Debug)]
pub struct B20CallSender<'a> {
    /// B-20 client signed by the benchmark admin.
    pub admin_client: &'a B20PrecompileClient<'a>,
    /// B-20 client signed by the benchmark spender.
    pub spender_client: &'a B20PrecompileClient<'a>,
    /// Benchmark token admin address.
    pub admin: Address,
    /// Benchmark spender address.
    pub spender: Address,
    /// Benchmark B-20 token address.
    pub token: Address,
    /// Progress display updated as calls are sent.
    pub display: &'a BenchDisplay,
}

impl B20ZkProvingBench {
    /// Runs the B-20 ZK proving benchmark against local RPC endpoints and a dry-run prover.
    pub async fn run(config: B20ZkProvingConfig) -> Result<()> {
        let display = BenchDisplay::new("B-20 zk dry-run benchmark", WORKLOAD_TXS);

        display.setup_message("setup connecting to benchmark RPCs");
        let l2_provider = BenchProvider::connect_base(config.l2_rpc_url.clone());
        let rollup_provider = BenchProvider::connect_base(config.rollup_rpc_url.clone());

        let admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
            .wrap_err("failed to parse benchmark admin private key")?;
        let spender = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_7.private_key)
            .wrap_err("failed to parse benchmark spender private key")?;
        display.setup_message("setup waiting for funded benchmark accounts");
        BenchProvider::wait_for_balances(
            &l2_provider,
            [admin.address(), spender.address()],
            config.block_poll_interval,
            config.account_funding_timeout,
        )
        .await?;

        let b20 = B20PrecompileClient::new(&l2_provider, &admin, config.l2_chain_id)
            .with_receipt_timeout(config.tx_receipt_timeout);
        let b20_spender = B20PrecompileClient::new(&l2_provider, &spender, config.l2_chain_id)
            .with_receipt_timeout(config.tx_receipt_timeout);
        display.setup_message("setup ensuring B-20 features are active");
        Self::ensure_feature_active(&b20, ActivationFeature::B20Asset.id()).await?;

        display.setup_message("setup creating benchmark B-20 token");
        let token = Self::create_b20_token(&b20, admin.address()).await?;
        display.setup_message("setup waiting for benchmark token bytecode");
        b20.wait_for_token_code(token, config.tx_receipt_timeout, config.block_poll_interval)
            .await?;
        display.setup_done(token);

        let reports = B20CallSender {
            admin_client: &b20,
            spender_client: &b20_spender,
            admin: admin.address(),
            spender: spender.address(),
            token,
            display: &display,
        }
        .send_sequence()
        .await?;
        display.txs_done();
        let (first_block, last_block) = OperationReport::block_range(&reports)?;
        let stats = ZkProofBench::prove_safe_block_range_with_dry_run_stats(
            &rollup_provider,
            config.zk_prover_url.clone(),
            first_block,
            last_block,
            ZkProofBenchConfig {
                safe_l2_timeout: config.safe_l2_timeout,
                safe_l2_poll_interval: config.block_poll_interval,
                proof_timeout: config.proof_timeout,
                proof_poll_interval: config.proof_poll_interval,
            },
            &display,
        )
        .await?;
        display.proof_done(&stats);

        CycleReport::print_summary(
            "B-20 zk dry-run proof benchmark",
            first_block,
            last_block,
            &reports,
            &stats,
        )
    }

    /// Creates the benchmark B-20 token.
    pub async fn create_b20_token(
        client: &B20PrecompileClient<'_>,
        admin: Address,
    ) -> Result<Address> {
        let salt = B256::from(rand::random::<[u8; 32]>());
        let params = B20PrecompileClient::token_params(
            "ZK Proof B20",
            "ZKPB",
            admin,
            U256::from(INITIAL_SUPPLY),
            admin,
        );

        client.create_token(params, salt).await
    }

    /// Activates `feature` if it is not already active.
    pub async fn ensure_feature_active(
        client: &B20PrecompileClient<'_>,
        feature: B256,
    ) -> Result<()> {
        let output = client
            .call(
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::isActivatedCall { feature },
            )
            .await?;
        let is_active = IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref())
            .wrap_err("failed to decode activation registry state")?;
        if !is_active {
            client.activate_feature(feature).await?;
        }
        Ok(())
    }
}

impl B20CallSender<'_> {
    /// Sends the fixed B-20 call sequence used by the proof benchmark.
    pub async fn send_sequence(&self) -> Result<Vec<OperationReport>> {
        let mut reports = Vec::new();

        reports.push(
            self.send_call(
                self.admin_client,
                "transfer",
                IB20::transferCall {
                    to: ANVIL_ACCOUNT_6.address,
                    amount: U256::from(TRANSFER_AMOUNT),
                },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "transferWithMemo",
                IB20::transferWithMemoCall {
                    to: ANVIL_ACCOUNT_6.address,
                    amount: U256::from(TRANSFER_WITH_MEMO_AMOUNT),
                    memo: B256::repeat_byte(0x20),
                },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "approve",
                IB20::approveCall { spender: self.spender, amount: U256::from(ALLOWANCE_AMOUNT) },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.spender_client,
                "transferFrom",
                IB20::transferFromCall {
                    from: self.admin,
                    to: ANVIL_ACCOUNT_6.address,
                    amount: U256::from(TRANSFER_FROM_AMOUNT),
                },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.spender_client,
                "transferFromWithMemo",
                IB20::transferFromWithMemoCall {
                    from: self.admin,
                    to: ANVIL_ACCOUNT_6.address,
                    amount: U256::from(TRANSFER_FROM_WITH_MEMO_AMOUNT),
                    memo: B256::repeat_byte(0x21),
                },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "updateSupplyCap",
                IB20::updateSupplyCapCall { newSupplyCap: U256::from(UPDATED_SUPPLY_CAP) },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "grantRole(metadata)",
                IB20::grantRoleCall { role: B20TokenRole::Metadata.id(), account: self.admin },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "updateContractURI",
                IB20::updateContractURICall { newURI: HEAVY_CONTRACT_URI.to_string() },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "updateName",
                IB20::updateNameCall { newName: "ZK Proof B20 Heavy Metadata".to_string() },
            )
            .await?,
        );
        reports.push(
            self.send_call(
                self.admin_client,
                "updateSymbol",
                IB20::updateSymbolCall { newSymbol: "ZKPH".to_string() },
            )
            .await?,
        );

        Ok(reports)
    }

    /// Sends a signed B-20 call and records its gas, block, and tracker metadata.
    pub async fn send_call<C>(
        &self,
        client: &B20PrecompileClient<'_>,
        operation: &'static str,
        call: C,
    ) -> Result<OperationReport>
    where
        C: SolCall,
    {
        let input = call.abi_encode();
        let tracker_key = IB20::IB20Calls::abi_decode(&input)
            .map_err(|_| eyre::eyre!("failed to decode B-20 cycle tracker key for {operation}"))?
            .as_label();
        self.display.tx_started(operation);
        let receipt = client.send_call_receipt(self.token, call, operation).await?;
        let report = OperationReport::from_receipt(operation, tracker_key, receipt)?;
        self.display.tx_done(&report);
        Ok(report)
    }
}
