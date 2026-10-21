//! Fork-aware gas budgeting shared by load-test and benchmark transaction paths.

use alloy_primitives::{Address, U64, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;

use super::{QueryProvider, RpcResultExt};
use crate::{BaselineError, Result};

#[derive(Debug, serde::Deserialize)]
struct AdmissionBlock {
    #[serde(rename = "gasLimit")]
    gas_limit: U64,
}

/// Estimates Amsterdam transaction budgets instead of maintaining workload-specific prices.
///
/// Detection uses an EOA self-transfer: 21,000 before Amsterdam, 12,000 under
/// EIP-2780. This needs only standard ETH RPC, not a chain ID or a debug API.
/// The selected fork is held constant for one run; do not benchmark across a fork.
#[derive(Debug, Clone)]
pub struct GasEstimator {
    client: QueryProvider,
    amsterdam: bool,
}

impl GasEstimator {
    /// Detects the active gas schedule using the funded, non-delegated funding EOA.
    pub async fn detect(client: QueryProvider, funder: Address) -> Result<Self> {
        let request = TransactionRequest {
            from: Some(funder),
            to: Some(funder.into()),
            value: Some(U256::ZERO),
            ..Default::default()
        };
        let estimate: U64 = client.client().request("eth_estimateGas", (&request,)).await
            .rpc("detect load-test gas schedule")?;
        let amsterdam = match estimate.to::<u64>() {
            12_000 => true,
            21_000 => false,
            other => return Err(BaselineError::Config(format!(
                "unexpected EOA self-transfer estimate {other}; use a funded, non-delegated funding EOA and an RPC returning exact transfer estimates"
            ))),
        };
        tracing::info!(amsterdam, self_transfer_gas = %estimate, "detected load-test gas schedule");
        Ok(Self { client, amsterdam })
    }

    /// Whether this run uses the Amsterdam budgeting path.
    pub const fn is_amsterdam(&self) -> bool {
        self.amsterdam
    }

    /// Leaves pre-Amsterdam requests byte-for-byte unchanged. For Amsterdam,
    /// estimates the actual payload (including CREATE, new accounts/slots and
    /// calldata floors), without the legacy gas ceiling. Reverts remain errors;
    /// neither workload calldata nor validity predicates are repaired or dropped.
    pub async fn apply(
        &self,
        mut request: TransactionRequest,
        from: Address,
    ) -> Result<TransactionRequest> {
        if !self.amsterdam {
            return Ok(request);
        }
        let mut simulation = request.clone();
        simulation.from = Some(from);
        simulation.gas = None;
        // Presigned transactions have future nonces. Estimate against canonical
        // state without requiring those predecessor nonces to be mined first.
        simulation.nonce = None;
        simulation.max_fee_per_gas = None;
        simulation.max_priority_fee_per_gas = None;
        simulation.gas_price = None;
        let estimate: U64 = self.client.client().request("eth_estimateGas", (&simulation,)).await
            .rpc("estimate Amsterdam workload gas (payload must succeed)")?;
        let block: Option<AdmissionBlock> = self.client.client()
            .request("eth_getBlockByNumber", ("latest", false)).await
            .rpc("read Amsterdam block admission capacity")?;
        let block_limit = block.ok_or_else(|| BaselineError::Rpc(
            "latest block missing while estimating admission capacity".into(),
        ))?.gas_limit.to();
        // A presigned ERC20 recipient can be drained before this transfer lands.
        // Reserve one fresh balance slot even when canonical estimation saw it
        // populated. Double-reserving an already-fresh slot is conservative.
        let estimate = Self::transfer_estimate(&request, estimate.to())?;
        request.gas = Some(Self::budget(request.gas.unwrap_or_default(), estimate, block_limit)?);
        Ok(request)
    }

    /// Estimates a setup/cleanup request outside the measured runner.
    pub async fn setup(
        client: &QueryProvider,
        request: TransactionRequest,
        from: Address,
    ) -> Result<TransactionRequest> {
        Self::detect(client.clone(), from).await?.apply(request, from).await
    }

    fn transfer_estimate(request: &TransactionRequest, estimate: u64) -> Result<u64> {
        let input = request.input.input();
        let transfer = input.is_some_and(|data| {
            data.len() == 68 && data[..4] == [0xa9, 0x05, 0x9c, 0xbb]
        });
        // EIP-8037: one 64-byte storage slot at CPSB 1,530.
        estimate.checked_add(if transfer { 64 * 1_530 } else { 0 })
            .ok_or_else(|| BaselineError::Config("Amsterdam transfer gas estimate overflow".into()))
    }

    /// Adds 25% headroom, bounded by block admission (not the regular-only cap).
    /// A legacy hint can be reduced; a successful estimate must never be reduced.
    pub fn budget(legacy: u64, estimate: u64, block_limit: u64) -> Result<u64> {
        if estimate > block_limit {
            return Err(BaselineError::Config(format!(
                "Amsterdam workload needs {estimate} gas, exceeding block admission capacity {block_limit}",
            )));
        }
        let padded = estimate.saturating_add(estimate.div_ceil(4));
        Ok(legacy.max(padded).min(block_limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use crate::rpc::RpcProviders;

    #[test]
    fn amsterdam_gas_budget_exact_values() {
        // New-account transfer: 21,000 regular + 120 * 1,530 state.
        assert_eq!(GasEstimator::budget(21_000, 204_600, 120_000_000).unwrap(), 255_750, "fresh ETH recipient plus 25% headroom");
        assert_eq!(GasEstimator::budget(65_000, 143_092, 120_000_000).unwrap(), 178_865, "fresh ERC20 balance slot estimate plus headroom");
        assert_eq!(GasEstimator::budget(122_000, 455_103, 120_000_000).unwrap(), 568_879, "four fresh SSTORE slots plus headroom");
        // EIP-7976: the floor counts zero and nonzero bytes equally at 64.
        assert_eq!(GasEstimator::budget(31_240, 15_000 + 256 * 64, 120_000_000).unwrap(), 39_230, "256-byte zero-value call floor plus headroom");
        assert_eq!(GasEstimator::budget(100_000, 12_000, 120_000_000).unwrap(), 100_000, "larger admissible existing budgets are retained");
        assert_eq!(GasEstimator::budget(30_000, 97_920_000, 120_000_000).unwrap(), 120_000_000, "1000 fresh slots retain successful estimate while headroom stops at block capacity");
        assert_eq!(GasEstimator::budget(130_000_000, 97_920_000, 120_000_000).unwrap(), 120_000_000, "oversized legacy hint must not defeat admission");
        assert_eq!(GasEstimator::budget(0, 120_000_000, 120_000_000).unwrap(), 120_000_000, "exact block boundary remains admissible above the regular-only cap");
        assert_eq!(GasEstimator::budget(0, 120_000_001, 120_000_000).unwrap_err().to_string(), "configuration error: Amsterdam workload needs 120000001 gas, exceeding block admission capacity 120000000", "impossible estimates must error rather than silently underfund execution");
    }

    #[test]
    fn amsterdam_gas_budget_erc20_ring_refill() {
        let mut input = vec![0; 68];
        input[..4].copy_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
        let request = TransactionRequest { input: input.into(), gas: Some(65_000), ..Default::default() };
        // B starts populated, B->C drains it, then A->B refills its fresh slot.
        let budgets: Vec<_> = [45_000, 142_920, 45_000].into_iter().map(|estimate| {
            GasEstimator::budget(65_000, GasEstimator::transfer_estimate(&request, estimate).unwrap(), 120_000_000).unwrap()
        }).collect();
        assert_eq!(budgets, [178_650, 301_050, 178_650], "each presigned transfer covers a 97,920-gas fresh slot regardless of the estimation-time balance");
        assert_eq!(GasEstimator::transfer_estimate(&request, 45_000).unwrap(), 142_920, "existing-slot estimate must reserve the exact Amsterdam fresh-slot charge before padding");
    }

    #[tokio::test]
    async fn amsterdam_gas_budget_rpc_admission_boundaries() {
        for (estimate, expected) in [(97_920_000u64, Some(120_000_000)), (120_000_001, None)] {
            let server = MockServer::start();
            let simulation = server.mock(|when, then| {
                when.method(POST).body_includes("eth_estimateGas");
                then.json_body(serde_json::json!({"jsonrpc":"2.0","id":0,"result":format!("0x{estimate:x}")}));
            });
            let admission = server.mock(|when, then| {
                when.method(POST).json_body(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}));
                then.json_body(serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"gasLimit":"0x7270e00"}}));
            });
            let estimator = GasEstimator { client: RpcProviders::query(server.url("/").parse().unwrap()).unwrap(), amsterdam: true };
            let result = estimator.apply(TransactionRequest { gas: Some(23_030_000), ..Default::default() }, Address::ZERO).await;
            if let Some(expected) = expected {
                assert_eq!(result.unwrap().gas, Some(expected), "RPC admission capacity must bound headroom, without applying the 2^24 regular cap");
            } else {
                assert_eq!(result.unwrap_err().to_string(), "configuration error: Amsterdam workload needs 120000001 gas, exceeding block admission capacity 120000000", "successful simulation above admission capacity must fail clearly");
            }
            simulation.assert_calls(1);
            admission.assert_calls(1);
        }
    }

    #[tokio::test]
    async fn amsterdam_gas_budget_does_not_hide_reverts() {
        let server = MockServer::start();
        let rejection = server.mock(|when, then| {
            when.method(POST).body_includes("eth_estimateGas");
            then.json_body(serde_json::json!({
                "jsonrpc":"2.0", "id":0,
                "error":{"code":3,"message":"execution reverted: insufficient token balance"}
            }));
        });
        let estimator = GasEstimator {
            client: RpcProviders::query(server.url("/").parse().unwrap()).unwrap(),
            amsterdam: true,
        };
        let request = TransactionRequest { gas: Some(65_000), ..Default::default() };
        let error = estimator.apply(request, Address::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("insufficient token balance"), "preserve the node's revert reason: {error}");
        rejection.assert_calls(1);
    }

    #[tokio::test]
    async fn amsterdam_gas_budget_preserves_every_payload() {
        use crate::workload::{Payload, SeededRng, TransferPayload, Erc20Payload,
            StoragePayload, DoubleCounterPayload, CalldataPayload, OsakaPayload,
            PrecompilePayload, B20TransferPayload, UniswapV3Payload};
        use crate::OsakaTarget;
        use alloy_primitives::B256;
        use revm::precompile::PrecompileId;

        let address = Address::repeat_byte(0x11);
        let payloads: Vec<Box<dyn Payload>> = vec![
            Box::new(TransferPayload::default()),
            Box::new(Erc20Payload::new(address, U256::from(1), U256::from(1))),
            Box::new(StoragePayload::new(address, 100)),
            Box::new(DoubleCounterPayload::new(address)),
            Box::new(CalldataPayload::new(256)),
            Box::new(OsakaPayload::new(OsakaTarget::Clz)),
            Box::new(OsakaPayload::new(OsakaTarget::P256verifyOsaka)),
            Box::new(PrecompilePayload::with_options(PrecompileId::Blake2F, Some(400_000), 1, None)),
            Box::new(B20TransferPayload::new(B256::ZERO, U256::from(1), U256::from(1))),
            Box::new(UniswapV3Payload::new(address, address, address, 500, U256::from(1), U256::from(1), None)),
        ];
        for payload in payloads {
            for amsterdam in [false, true] {
                let server = MockServer::start();
                let client = RpcProviders::query(server.url("/").parse().unwrap()).unwrap();
                let estimator = GasEstimator { client, amsterdam };
                let request = payload.generate(&mut SeededRng::new(1), address, Address::ZERO);
                let mut simulation = request.clone();
                simulation.gas = None;
                simulation.from = Some(address);
                let admission = server.mock(|when, then| {
                    when.method(POST).body_includes("eth_getBlockByNumber");
                    then.json_body(serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"gasLimit":"0x7270e00"}}));
                });
                let estimate = server.mock(|when, then| {
                    when.method(POST).json_body(serde_json::json!({
                        "jsonrpc":"2.0", "id":0, "method":"eth_estimateGas", "params":[simulation]
                    }));
                    then.json_body(serde_json::json!({"jsonrpc":"2.0","id":0,"result":"0x989680"}));
                });
                let actual = estimator.apply(request.clone(), address).await.unwrap();
                let mut expected = request;
                if amsterdam {
                    expected.gas = Some(if matches!(payload.name(), "erc20" | "b20") { 12_622_400 } else { 12_500_000 });
                }
                assert_eq!(actual, expected, "{}: only the Amsterdam declared budget changes", payload.name());
                estimate.assert_calls(usize::from(amsterdam));
                admission.assert_calls(usize::from(amsterdam));
            }
        }
    }

    #[tokio::test]
    async fn amsterdam_gas_budget_rpc_and_legacy_preservation() {
        for (self_gas, active) in [("0x5208", false), ("0x2ee0", true)] {
            let server = MockServer::start();
            let mut detection = server.mock(|when, then| {
                when.method(POST).body_includes("eth_estimateGas").body_includes("\"value\":\"0x0\"");
                then.json_body(serde_json::json!({"jsonrpc":"2.0","id":0,"result":self_gas}));
            });
            let estimator = GasEstimator::detect(RpcProviders::query(server.url("/").parse().unwrap()).unwrap(), Address::ZERO).await.unwrap();
            detection.assert_calls(1);
            detection.delete();
            let payload = server.mock(|when, then| {
                when.method(POST).body_includes("eth_estimateGas").body_includes("\"value\":\"0x1\"");
                then.json_body(serde_json::json!({"jsonrpc":"2.0","id":1,"result":"0x31f38"}));
            });
            let admission = server.mock(|when, then| {
                when.method(POST).body_includes("eth_getBlockByNumber");
                then.json_body(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"gasLimit":"0x7270e00"}}));
            });
            let request = TransactionRequest { gas: Some(21_000), value: Some(U256::from(1)), nonce: Some(8), ..Default::default() };
            let result = estimator.apply(request.clone(), Address::ZERO).await.unwrap();
            assert_eq!(result.gas, Some(if active { 255_750 } else { 21_000 }), "fork-correct declared transfer budget");
            assert_eq!(result.nonce, request.nonce, "estimation must not alter the signed nonce");
            assert_eq!(result.value, request.value, "estimation must not alter workload value");
            payload.assert_calls(usize::from(active));
            admission.assert_calls(usize::from(active));
            if !active { assert_eq!(result, request, "legacy requests are unchanged without per-payload RPC"); }
        }
    }
}
