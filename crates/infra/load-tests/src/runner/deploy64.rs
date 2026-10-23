//! Independently paced 64 `KiB` deployments, excluded from ordinary-mix calibration and metrics.

use std::time::Duration;

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256, bytes};
use alloy_provider::Provider;
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    BaselineError, Result,
    rpc::{BaseFeeExt, RpcProviders, RpcResultExt, create_wallet_provider},
};

/// EIP-7954's maximum deployed runtime size at Amsterdam.
const RUNTIME_SIZE: usize = 65_536;
/// EIP-8037 state for one new account and 64 `KiB` of code at the pinned L1 CPSB.
const STATE_GAS: u64 = 100_453_680;
/// EIP-7976 calldata floor, including the 14-byte initcode prefix.
const REGULAR_FLOOR: u64 = 4_219_200;
/// Exact charged gas for this initcode (eight nonzero runtime bytes, no refunds).
const RECEIPT_GAS: u64 = 100_776_978;
/// State plus the regular floor, with one percent rounded-up admission headroom.
const GAS_LIMIT: u64 = (STATE_GAS + REGULAR_FLOOR) + (STATE_GAS + REGULAR_FLOOR).div_ceil(100);

/// Optional side workload; the ordinary mix and its gas/s target are unchanged.
/// Enable at the top level of the existing workload YAML, not in `transactions`:
///
/// ```yaml
/// deploy64:
///   interval_secs: 10
///   priority_fee_wei: 1000000000
/// ```
///
/// Uses the exclusively owned funding account, which must retain enough ETH for deployment
/// fees after setup. The first attempt is after one interval. Ordinary-mix `target_gps` excludes
/// this lane: add the separate `offered_gas`/`gas_used` records when computing combined work.
/// A fixed 105,719,609-gas declaration covers the pinned Amsterdam schedule with 1% headroom;
/// lower block limits are rejected, not clamped. No activation schedule is changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Deploy64Config {
    /// Minimum pause between attempts; confirmation time is additional, with no catch-up burst.
    pub interval_secs: u64,
    /// Priority fee in wei. Defaults above the ordinary mix's base-fee/10 tip.
    pub priority_fee_wei: u128,
}

impl Default for Deploy64Config {
    fn default() -> Self {
        Self { interval_secs: 10, priority_fee_wei: 1_000_000_000 }
    }
}

impl Deploy64Config {
    pub(super) fn validate(&self) -> Result<()> {
        if self.interval_secs == 0 || self.priority_fee_wei == 0 {
            return Err(BaselineError::Config(
                "deploy64 interval_secs and priority_fee_wei must be > 0".into(),
            ));
        }
        Ok(())
    }
}

/// One deployment, reported separately from ordinary-mix offered/achieved gas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Deploy64Result {
    /// Submitted transaction hash, if accepted by the RPC.
    pub tx_hash: Option<B256>,
    /// Declared gas (admission reservation, not the gas offered as work).
    pub gas_limit: u64,
    /// Expected charged gas, counted only for accepted submissions.
    pub offered_gas: u64,
    /// Canonical inclusion block, if confirmed.
    pub block_number: Option<u64>,
    /// Receipt status, if confirmed.
    pub status: Option<bool>,
    /// Actual receipt gas, including state gas.
    pub gas_used: Option<u64>,
    /// CREATE address, if returned in the receipt.
    pub contract_address: Option<Address>,
    /// Code length fetched at the inclusion block.
    pub code_size: Option<usize>,
    /// Whether calling the deployed code returned the expected 32-byte value 42.
    pub call_verified: bool,
    /// Submission, confirmation or verification failure; never silently dropped.
    pub error: Option<String>,
}

/// CREATE initcode copying an exactly 64 `KiB` runtime that returns uint256(42).
/// CREATE addresses remain unique through the exclusive deployer's increasing nonce.
fn initcode() -> Bytes {
    let mut code = bytes!("62010000600e5f39620100005ff3").to_vec();
    code.extend_from_slice(&[0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
    code.resize(14 + RUNTIME_SIZE, 0);
    code.into()
}

/// Refuses unsupported forks and insufficient block capacity without shrinking the payload.
pub(super) async fn preflight(
    endpoint: url::Url,
    signer: &PrivateKeySigner,
    config: &Deploy64Config,
    max_gas_price: u128,
) -> Result<()> {
    config.validate()?;
    if config.priority_fee_wei > max_gas_price {
        return Err(BaselineError::Config("deploy64 priority fee exceeds max gas price".into()));
    }
    let provider = RpcProviders::query(endpoint)?;
    if !crate::GasEstimator::detect(provider.clone(), signer.address()).await?.is_amsterdam() {
        return Err(BaselineError::Config(
            "deploy64 requires Amsterdam; no fork timing is changed".into(),
        ));
    }
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .hashes()
        .await
        .rpc("read deploy64 block capacity")?
        .ok_or_else(|| BaselineError::Rpc("deploy64 latest block unavailable".into()))?;
    check_capacity(block.header.gas_limit)?;
    // Do not estimate this CREATE: the RPC simulation ceiling can be below the block limit.
    // The explicit, production-EVM-tested budget also prevents the wallet gas filler from
    // invoking eth_estimateGas; pool admission still enforces the actual block limit.
    Ok(())
}

fn check_capacity(block_limit: u64) -> Result<()> {
    if GAS_LIMIT > block_limit {
        return Err(BaselineError::Config(format!(
            "deploy64 needs {GAS_LIMIT} declared gas, exceeding block limit {block_limit}"
        )));
    }
    Ok(())
}

/// Runs one in-flight CREATE at a time on the exclusive funder, not a presigned mix sender.
/// Stop ends submissions, but an accepted transaction is allowed a bounded receipt drain.
pub(super) async fn run(
    endpoint: url::Url,
    signer: PrivateKeySigner,
    chain_id: u64,
    config: Deploy64Config,
    max_gas_price: u128,
    stop: CancellationToken,
) -> Vec<Deploy64Result> {
    let provider = RpcProviders::query(endpoint.clone()).expect("validated endpoint");
    let wallet = create_wallet_provider(endpoint, EthereumWallet::from(signer.clone()));
    let interval = Duration::from_secs(config.interval_secs);
    let mut results = Vec::new();
    // First deploy follows one full interval, not setup/calibration or the warmup prefill.
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            () = tokio::time::sleep(interval) => {}
        }
        let mut result = Deploy64Result { gas_limit: GAS_LIMIT, ..Default::default() };
        let attempt: Result<()> = async {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .hashes()
                .await
                .rpc("read deploy64 block capacity")?
                .ok_or_else(|| BaselineError::Rpc("deploy64 latest block unavailable".into()))?;
            check_capacity(block.header.gas_limit)?;
            let base_fee = provider.get_base_fee().await?;
            // Use a stronger tip than the ordinary base_fee/10 lane, subject to the same cap.
            let tip = config.priority_fee_wei.max(base_fee / 5);
            let max_fee = base_fee.saturating_mul(2).saturating_add(tip).min(max_gas_price);
            if max_fee < base_fee.saturating_add(tip) {
                return Err(BaselineError::Config(
                    "deploy64 fee cap cannot cover base fee plus priority fee".into(),
                ));
            }
            if provider.get_balance(signer.address()).await.rpc("read deploy64 balance")?
                < U256::from(GAS_LIMIT) * U256::from(max_fee)
            {
                return Err(BaselineError::Config(
                    "deploy64 funder balance cannot cover declared gas at max fee".into(),
                ));
            }
            let nonce = provider
                .get_transaction_count(signer.address())
                .pending()
                .await
                .rpc("read deploy64 nonce")?;
            let request = TransactionRequest::default()
                .with_deploy_code(initcode())
                .with_from(signer.address())
                .with_chain_id(chain_id)
                .with_nonce(nonce)
                .with_gas_limit(GAS_LIMIT)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(tip);
            let pending = wallet.send_transaction(request).await.rpc("submit deploy64 CREATE")?;
            result.tx_hash = Some(*pending.tx_hash());
            result.offered_gas = RECEIPT_GAS;
            let receipt = pending
                .with_timeout(Some(Duration::from_secs(60)))
                .get_receipt()
                .await
                .rpc("confirm deploy64 CREATE")?;
            result.block_number = receipt.block_number;
            result.status = Some(receipt.status());
            result.gas_used = Some(receipt.gas_used);
            result.contract_address = receipt.contract_address;
            if !receipt.status() {
                return Err(BaselineError::Transaction(
                    "deploy64 CREATE reverted or halted".into(),
                ));
            }
            let address = receipt.contract_address.ok_or_else(|| {
                BaselineError::Transaction("deploy64 receipt missing contract address".into())
            })?;
            let block = receipt.block_number.ok_or_else(|| {
                BaselineError::Transaction("deploy64 receipt missing block number".into())
            })?;
            let code = provider
                .get_code_at(address)
                .block_id(block.into())
                .await
                .rpc("read deploy64 code")?;
            result.code_size = Some(code.len());
            if code.len() != RUNTIME_SIZE {
                return Err(BaselineError::Transaction(format!(
                    "deploy64 code has {} bytes, expected {RUNTIME_SIZE}",
                    code.len()
                )));
            }
            let output = provider
                .call(TransactionRequest::default().with_to(address).into())
                .block(block.into())
                .await
                .rpc("call deploy64 runtime")?;
            result.call_verified = output.as_ref() == U256::from(42).to_be_bytes::<32>();
            if !result.call_verified {
                return Err(BaselineError::Transaction(
                    "deploy64 runtime did not return uint256(42)".into(),
                ));
            }
            Ok(())
        }
        .await;
        let failed = attempt.is_err();
        if let Err(error) = attempt {
            result.error = Some(error.to_string());
        }
        tracing::info!(?result, "deploy64 result (separate from ordinary mix)");
        results.push(result);
        // Never retry a timed-out nonce or hide a node admission/verification failure.
        if failed {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deploy64_wallet_sends_full_budget_without_estimation() {
        use httpmock::prelude::*;

        use crate::{Fees, PreparedTransaction, SubmissionPipeline, SubmitCohort};
        let server = MockServer::start();
        let signer = PrivateKeySigner::random();
        let prepared = PreparedTransaction {
            from: signer.address(),
            to: None,
            value: U256::ZERO,
            data: initcode(),
            gas_limit: GAS_LIMIT,
            estimated_gas: RECEIPT_GAS,
            validity: Vec::new(),
            cohort: SubmitCohort::Plain,
        };
        let signed = SubmissionPipeline::sign_at_nonce(
            &signer,
            &prepared,
            8453,
            7,
            Fees { max_fee: 2_000_000_000, priority_fee: 1_000_000_000 },
        )
        .expect("sign expected CREATE envelope");
        let hash = B256::repeat_byte(3);
        let send = server.mock(|when, then| {
            when.method(POST)
                .body_includes("eth_sendRawTransaction")
                .body_includes(signed.raw.to_string());
            then.json_body(serde_json::json!({"jsonrpc":"2.0","id":0,"result":hash}));
        });
        let estimate = server.mock(|when, then| {
            when.method(POST).body_includes("eth_estimateGas");
            then.status(500);
        });
        let provider = create_wallet_provider(
            server.url("/").parse().expect("mock URL"),
            EthereumWallet::from(signer.clone()),
        );
        let pending = provider
            .send_transaction(
                TransactionRequest::default()
                    .with_deploy_code(initcode())
                    .with_from(signer.address())
                    .with_chain_id(8453)
                    .with_nonce(7)
                    .with_gas_limit(GAS_LIMIT)
                    .with_max_fee_per_gas(2_000_000_000)
                    .with_max_priority_fee_per_gas(1_000_000_000),
            )
            .await
            .expect("computed above-cap budget must reach sendRawTransaction unchanged");
        assert_eq!(*pending.tx_hash(), hash, "RPC submission receives complete CREATE envelope");
        send.assert_calls(1);
        estimate.assert_calls(0);
    }

    #[tokio::test]
    async fn deploy64_rejects_pre_amsterdam_without_sending_create() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let probe = server.mock(|when, then| {
            when.method(POST).body_includes("eth_estimateGas");
            then.json_body(serde_json::json!({"jsonrpc":"2.0","id":0,"result":"0x5208"}));
        });
        let signer = PrivateKeySigner::random();
        let error = preflight(
            server.url("/").parse().expect("mock URL"),
            &signer,
            &Deploy64Config::default(),
            1_000_000_000_000,
        )
        .await
        .expect_err("Osaka must be rejected");
        assert_eq!(
            error.to_string(),
            "configuration error: deploy64 requires Amsterdam; no fork timing is changed",
            "no oversized deployment or activation workaround before Amsterdam"
        );
        probe.assert_calls(1);
    }

    #[tokio::test]
    async fn deploy64_cancelled_lane_submits_nothing() {
        let stop = CancellationToken::new();
        stop.cancel();
        let signer = PrivateKeySigner::random();
        let results = run(
            "http://localhost:1".parse().expect("valid unused endpoint"),
            signer,
            8453,
            Deploy64Config::default(),
            1_000_000_000_000,
            stop,
        )
        .await;
        assert!(
            results.is_empty(),
            "cancelled lane must stop before the first10second tick without any RPC submission"
        );
    }

    #[test]
    fn deploy64_exact_payload_budget_and_rate() {
        let code = initcode();
        assert_eq!(code.len(), 65_550, "14-byte initcode prefix plus full 64 KiB runtime");
        assert_eq!(
            &code[..14],
            &bytes!("62010000600e5f39620100005ff3")[..],
            "copy/return exactly 65536 bytes from offset14"
        );
        assert_eq!(
            &code[14..22],
            &[0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3],
            "runtime returns42"
        );
        assert!(code[22..].iter().all(|b| *b == 0), "remaining runtime is STOP padding");
        assert_eq!(STATE_GAS, (120 + 65_536) * 1_530, "account plus code state gas");
        assert_eq!(REGULAR_FLOOR, 24_000 + 65_550 * 64, "Amsterdam CREATE calldata floor");
        assert_eq!(RECEIPT_GAS, STATE_GAS + 323_298, "spent regular plus state, no refund");
        assert_eq!(GAS_LIMIT, 105_719_609, "state plus regular floor plus rounded-up1% margin");
        assert!(check_capacity(120_000_000).is_ok(), "fits120M block");
        assert!(check_capacity(150_000_000).is_ok(), "fits150M block");
        assert_eq!(
            check_capacity(100_000_000).unwrap_err().to_string(),
            "configuration error: deploy64 needs 105719609 declared gas, exceeding block limit 100000000",
            "reject instead of clamping an impossible budget"
        );
        assert_eq!(
            Deploy64Config::default().interval_secs,
            10,
            "default one submission per10seconds"
        );
        assert_eq!(Deploy64Config::default().priority_fee_wei, 1_000_000_000, "default tip1gwei");
        assert!(
            Deploy64Config { interval_secs: 0, ..Default::default() }.validate().is_err(),
            "zero interval rejected"
        );
    }

    #[test]
    fn deploy64_executes_at_amsterdam_with_regular_cap() {
        use alloy_evm::Evm;
        use base_common_evm::{
            BaseSpecId, BaseTransaction, BaseUpgrade, Builder, DefaultBase, L1BlockInfo,
        };
        use revm::{
            Context, DatabaseCommit,
            context::{BlockEnv, CfgEnv, TxEnv},
            database::InMemoryDB,
            state::AccountInfo,
        };

        for block_limit in [120_000_000, 150_000_000] {
            let caller = Address::repeat_byte(0x11);
            let mut db = InMemoryDB::default();
            db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::from(10u64).pow(U256::from(18)),
                    ..Default::default()
                },
            );
            let mut cfg = CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Denim));
            cfg.tx_gas_limit_cap = Some(1 << 24);
            assert_eq!(
                revm::context_interface::Cfg::max_code_size(&cfg),
                65_536,
                "Amsterdam runtime limit"
            );
            assert_eq!(
                revm::context_interface::Cfg::max_initcode_size(&cfg),
                131_072,
                "Amsterdam initcode limit"
            );
            let mut evm = Context::base()
                .with_db(db)
                .with_cfg(cfg)
                .with_block(BlockEnv { gas_limit: block_limit, ..Default::default() })
                .with_chain(L1BlockInfo {
                    l2_block: Some(U256::ZERO),
                    operator_fee_scalar: Some(U256::ZERO),
                    operator_fee_constant: Some(U256::ZERO),
                    ..Default::default()
                })
                .build_base()
                .with_inspector(revm::inspector::NoOpInspector);
            let tx = BaseTransaction::builder()
                .base(
                    TxEnv::builder()
                        .caller(caller)
                        .kind(alloy_primitives::TxKind::Create)
                        .gas_limit(GAS_LIMIT)
                        .data(initcode()),
                )
                .enveloped_tx(Some(bytes!("FACADE")))
                .build_fill();
            let result =
                evm.transact(tx).expect("above-cap total gas CREATE must be accepted under8037");
            assert!(result.result.is_success(), "64KiB CREATE must succeed: {:?}", result.result);
            assert_eq!(
                result.result.gas().tx_gas_used(),
                RECEIPT_GAS,
                "exact receipt state plus spent regular"
            );
            assert_eq!(
                result.result.gas().block_regular_gas_used(),
                REGULAR_FLOOR,
                "regular budget counts floor, not code state"
            );
            assert_eq!(
                result.result.gas().block_state_gas_used(),
                STATE_GAS,
                "full code and account state charge"
            );
            let address = caller.create(0);
            let code = result
                .state
                .get(&address)
                .expect("created account")
                .info
                .code
                .as_ref()
                .expect("runtime code")
                .original_bytes();
            assert_eq!(code.len(), RUNTIME_SIZE, "actual deployed code is exactly65536bytes");
            evm.db_mut().commit(result.state);
            let call = BaseTransaction::builder()
                .base(
                    TxEnv::builder()
                        .caller(caller)
                        .nonce(1)
                        .kind(address.into())
                        .gas_limit(100_000),
                )
                .enveloped_tx(Some(bytes!("FACADE")))
                .build_fill();
            let result = evm.transact(call).expect("call deployed64KiB contract").result;
            assert!(result.is_success(), "deployed runtime executes successfully");
            assert_eq!(
                result.output().expect("call output").as_ref(),
                &U256::from(42).to_be_bytes::<32>(),
                "actual runtime returns uint256(42)"
            );
        }
    }

    #[test]
    fn deploy64_config_does_not_change_ordinary_mix() {
        let config: crate::config::TestConfig = serde_yaml::from_str("transaction_submission_rpcs: [http://localhost:8545]\nchain_id: 8453\ndeploy64: {}\ntarget_gps: 400000000\ntransactions:\n  - weight: 100\n    type: transfer\n").expect("deployment config YAML");
        let load = config.to_load_config(None).expect("runtime config");
        assert_eq!(
            load.deploy64.expect("optional lane enabled").interval_secs,
            10,
            "default10second interval"
        );
        assert_eq!(load.target_gps, Some(400_000_000), "ordinary mix target unaffected");
        assert_eq!(
            load.transactions.len(),
            1,
            "deployment never participates in weighted calibration"
        );
        assert!(
            matches!(load.transactions[0].tx_type, super::super::TxType::Transfer),
            "ordinary transfer retained"
        );
        let summary = crate::metrics::MetricsSummary {
            deploy64: vec![Deploy64Result {
                gas_used: Some(RECEIPT_GAS),
                code_size: Some(RUNTIME_SIZE),
                ..Default::default()
            }],
            ..Default::default()
        };
        let json = serde_json::to_value(summary).expect("result JSON");
        assert_eq!(json["deploy64"][0]["gas_used"], RECEIPT_GAS, "separate achieved receipt gas");
        assert_eq!(json["deploy64"][0]["code_size"], RUNTIME_SIZE, "per-deploy code size surfaced");
    }
}
