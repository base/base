//! Facilitator-submitted USDC EIP-3009 settlements using separately funded payer accounts.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, U256, address};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{Eip712Domain, SolCall, SolStruct, SolValue, sol};
use async_trait::async_trait;

use super::Payload;
use crate::{
    BaselineError, Result,
    rpc::RpcResultExt,
    workload::{ChainPrepContext, SeededRng},
};

/// Canonical native USDC proxy on Base mainnet.
pub const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

sol! {
    /// Payer-signed EIP-3009 typed authorization.
    struct TransferWithAuthorization {
        address from;
        address to;
        uint256 value;
        uint256 validAfter;
        uint256 validBefore;
        bytes32 nonce;
    }
    /// Legacy EOA signature entrypoint supported by real USDC.
    function transferWithAuthorization(address from, address to, uint256 value, uint256 validAfter, uint256 validBefore, bytes32 nonce, uint8 v, bytes32 r, bytes32 s);
    /// USDC blacklist view (V2.2 uses the high bit of its balance word).
    function isBlacklisted(address account) external view returns (bool);
}

/// Snapshot-derived signing context shared by measured and calibration generators.
#[derive(Debug, Clone)]
pub struct X402Context {
    /// Domain for the snapshot's chain and canonical USDC proxy.
    pub domain: Eip712Domain,
    /// Snapshot timestamp from which authorization validity is measured.
    pub timestamp: u64,
    /// Each facilitator maps to a different, already funded payer.
    pub payers: HashMap<Address, PrivateKeySigner>,
}

impl X402Context {
    /// Builds a USDC domain; setup checks it against the actual proxy.
    pub const fn domain(chain_id: u64) -> Eip712Domain {
        alloy_sol_types::eip712_domain! { name: "USD Coin", version: "2", chain_id: chain_id, verifying_contract: USDC, }
    }

    /// Installs payer mappings after snapshot balance and blacklist checks.
    pub fn new(chain_id: u64, timestamp: u64, payers: Vec<(Address, PrivateKeySigner)>) -> Self {
        Self { domain: Self::domain(chain_id), timestamp, payers: payers.into_iter().collect() }
    }

    /// Rejects payers whose USDC state cannot fund a settlement.
    pub fn validate_payer(balance: U256, blacklisted: bool) -> Result<()> {
        if blacklisted || balance.is_zero() {
            return Err(BaselineError::Config(
                "x402 requires an unblocked payer with USDC balance".into(),
            ));
        }
        Ok(())
    }
}

/// EIP-3009 settlement payload sharing snapshot preparation across reconstruction.
#[derive(Debug, Clone)]
pub struct X402Payload {
    /// Once-prepared state also retained by transaction configuration clones.
    pub context: Arc<OnceLock<X402Context>>,
}

impl X402Payload {
    /// Creates an already prepared payload, useful for offline signing tests.
    pub fn new(context: X402Context) -> Self {
        Self { context: Arc::new(OnceLock::from(context)) }
    }

    /// Verifies real snapshot USDC and maps each facilitator to its ring partner's signer.
    pub async fn prepare_context(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        if self.context.get().is_some() {
            return Ok(());
        }
        if ctx.chain_id != 8453 || ctx.accounts.len() < 3 {
            return Err(BaselineError::Config(
                "x402 requires a Base mainnet snapshot and at least three accounts".into(),
            ));
        }
        let setup = ctx.real_token_setup.ok_or_else(|| {
            BaselineError::Config("x402 requires real_token_setup for USDC".into())
        })?;
        if setup.pair_token.token != USDC {
            return Err(BaselineError::Config(
                "x402 real_token_setup must acquire canonical USDC".into(),
            ));
        }
        super::real_token_lifecycle::prepare(ctx, setup).await?;
        let domain_bytes = ctx
            .client
            .call(
                TransactionRequest::default()
                    .with_to(USDC)
                    .with_input(alloy_primitives::Bytes::from_static(&[0x36, 0x44, 0xe5, 0x15]))
                    .into(),
            )
            .await
            .rpc("read USDC domain separator")?;
        if domain_bytes.as_ref() != Self::expected_domain(ctx.chain_id).as_slice() {
            return Err(BaselineError::Config(
                "snapshot USDC domain separator does not match x402 domain".into(),
            ));
        }
        let block: serde_json::Value = ctx
            .client
            .client()
            .request("eth_getBlockByNumber", ("latest", false))
            .await
            .rpc("read x402 snapshot timestamp")?;
        let timestamp = block["timestamp"]
            .as_str()
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .ok_or_else(|| BaselineError::Config("missing snapshot timestamp".into()))?;
        let accounts = ctx.accounts.accounts();
        let mut payers = Vec::with_capacity(accounts.len());
        for (index, facilitator) in accounts.iter().enumerate() {
            let payer = &accounts[(index + accounts.len() - 1) % accounts.len()];
            let balance = ctx
                .client
                .call(
                    TransactionRequest::default()
                        .with_to(USDC)
                        .with_input(crate::workload::encode_erc20_balance_of(payer.address))
                        .into(),
                )
                .await
                .rpc("read x402 payer USDC balance")?;
            let blacklist = ctx
                .client
                .call(
                    TransactionRequest::default()
                        .with_to(USDC)
                        .with_input(isBlacklistedCall { account: payer.address }.abi_encode())
                        .into(),
                )
                .await
                .rpc("read x402 payer blacklist")?;
            let balance =
                U256::abi_decode(&balance).map_err(|e| BaselineError::Config(e.to_string()))?;
            let blacklisted =
                bool::abi_decode(&blacklist).map_err(|e| BaselineError::Config(e.to_string()))?;
            X402Context::validate_payer(balance, blacklisted)?;
            payers.push((facilitator.address, payer.signer.clone()));
        }
        self.context
            .set(X402Context::new(ctx.chain_id, timestamp, payers))
            .map_err(|_| BaselineError::Config("x402 prepared twice concurrently".into()))?;
        Ok(())
    }

    /// Returns the expected proxy domain separator for preparation checks.
    pub fn expected_domain(chain_id: u64) -> B256 {
        X402Context::domain(chain_id).hash_struct()
    }
}

#[async_trait]
impl Payload for X402Payload {
    fn name(&self) -> &'static str {
        "x402"
    }
    fn uses_runner_recipient(&self) -> bool {
        true
    }

    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(300), self.prepare_context(ctx))
            .await
            .map_err(|_| BaselineError::Config("x402 setup timed out".into()))?
    }

    fn generate(&self, _rng: &mut SeededRng, from: Address, to: Address) -> TransactionRequest {
        let context = self.context.get().expect("x402 setup must complete before generation");
        let payer = context.payers.get(&from).expect("x402 facilitator must have a prepared payer");
        // OS-random nonces do not reset with the seeded workload RNG during calibration.
        let authorization = TransferWithAuthorization {
            from: payer.address(),
            to,
            value: U256::from(1),
            validAfter: U256::from(context.timestamp.saturating_sub(1)),
            validBefore: U256::from(context.timestamp.saturating_add(604_800)),
            nonce: B256::random(),
        };
        let signature = payer
            .sign_hash_sync(&authorization.eip712_signing_hash(&context.domain))
            .expect("local payer signing must succeed");
        let call = transferWithAuthorizationCall {
            from: authorization.from,
            to,
            value: authorization.value,
            validAfter: authorization.validAfter,
            validBefore: authorization.validBefore,
            nonce: authorization.nonce,
            v: 27 + u8::from(signature.v()),
            r: B256::from(signature.r()),
            s: B256::from(signature.s()),
        };
        TransactionRequest::default()
            .with_to(USDC)
            .with_input(call.abi_encode())
            .with_gas_limit(150_000)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address, b256};
    use alloy_signer_local::PrivateKeySigner;
    use alloy_sol_types::{SolCall, SolStruct};
    use httpmock::{Method::POST, MockServer};

    use super::*;
    use crate::{
        RpcProviders,
        workload::{AccountPool, ChainPrepOutputs},
    };

    /// Preparation must read the funded snapshot balances and reject real blacklist responses.
    #[tokio::test]
    async fn preparation_checks_snapshot_usdc_state() {
        for (balance, blacklisted, succeeds) in
            [(10_000_000u64, false, true), (0, false, false), (10_000_000, true, false)]
        {
            let server = MockServer::start();
            for (selector, value) in [
                ("3644e515", format!("{:#x}", X402Context::domain(8453).hash_struct())),
                ("70a08231", format!("{:#066x}", U256::from(balance))),
                ("fe575a87", format!("{:#066x}", U256::from(u8::from(blacklisted)))),
            ] {
                server.mock(|when, then| {
                    when.method(POST).body_includes(selector);
                    then.status(200)
                        .json_body(serde_json::json!({"jsonrpc":"2.0", "id":0, "result":value}));
                });
            }
            server.mock(|when, then| {
                when.method(POST).body_includes("eth_getBlockByNumber");
                then.status(200).json_body(serde_json::json!({"jsonrpc":"2.0", "id":0, "result":{"timestamp":"0x6b49d200"}}));
            });
            let config: crate::config::TestConfig = serde_yaml::from_str(include_str!(
                "../../../examples/mainnet-mix-mainnet-snapshot.yaml"
            ))
            .expect("mainnet token setup must parse");
            let setup =
                config.parse_real_token_setup(8453).expect("real token setup must validate");
            let accounts = AccountPool::new(7, 3).expect("three distinct accounts must derive");
            let client =
                RpcProviders::query(server.base_url().parse().expect("mock URL must parse"))
                    .expect("query provider must build");
            let mut ctx = ChainPrepContext {
                client: &client,
                accounts: &accounts,
                chain_id: 8453,
                max_gas_price: 1_000_000_000,
                primary_submission_rpc: server
                    .base_url()
                    .parse()
                    .expect("submission URL must parse"),
                concurrency: 1,
                b20_mint: U256::ZERO,
                real_token_setup: setup.as_ref(),
                swap_routers: vec![],
                // Existing swap acquisition has completed; exercise x402's post-acquisition checks.
                outputs: ChainPrepOutputs { real_tokens_prepared: true, ..Default::default() },
            };
            let payload = X402Payload { context: Arc::default() };
            let result = payload.prepare(&mut ctx).await;
            assert_eq!(
                result.is_ok(),
                succeeds,
                "snapshot balance/blacklist must govern preparation: {result:?}"
            );
            assert_eq!(
                payload.context.get().is_some(),
                succeeds,
                "invalid state must never install signing context"
            );
            if succeeds {
                let context =
                    payload.context.get().expect("successful preparation must install context");
                assert_eq!(
                    context.payers.len(),
                    3,
                    "each facilitator must have exactly one funded payer"
                );
                for (index, facilitator) in accounts.accounts().iter().enumerate() {
                    let payer = context
                        .payers
                        .get(&facilitator.address)
                        .expect("facilitator must be mapped");
                    assert_eq!(
                        payer.address(),
                        accounts.accounts()[(index + 2) % 3].address,
                        "payer must be preceding funded account"
                    );
                    assert_ne!(
                        payer.address(),
                        accounts.accounts()[(index + 1) % 3].address,
                        "payer must not be merchant"
                    );
                }
            }
        }
    }

    /// Setup must reject blacklisted or empty payers and accept a funded, unblocked payer.
    #[test]
    fn funding_requires_unblocked_usdc_balance() {
        assert!(
            X402Context::validate_payer(U256::ZERO, false).is_err(),
            "zero USDC cannot fund a settlement"
        );
        assert!(
            X402Context::validate_payer(U256::from(10_000_000), true).is_err(),
            "blacklisted payer must be rejected despite balance"
        );
        assert!(
            X402Context::validate_payer(U256::from(10_000_000), false).is_ok(),
            "funded unblocked payer must pass setup"
        );
    }

    /// The mainnet profile must include x402 while preserving exact normalized total weight.
    #[test]
    fn mainnet_mix_contains_x402_with_normalized_weight() {
        let config: crate::config::TestConfig = serde_yaml::from_str(include_str!(
            "../../../examples/mainnet-mix-mainnet-snapshot.yaml"
        ))
        .expect("mainnet mix must parse");
        let setup = config.real_token_setup.clone().expect("mainnet mix must acquire real USDC");
        let config = config.to_load_config(None).expect("mainnet mix must convert");
        assert_eq!(
            config.transactions.iter().map(|tx| tx.weight).sum::<u32>(),
            1000,
            "mainnet mix must normalize to exactly 1000"
        );
        let x402 = config
            .transactions
            .iter()
            .filter(|tx| matches!(tx.tx_type, crate::runner::TxType::X402 { .. }))
            .collect::<Vec<_>>();
        assert_eq!(x402.len(), 1, "exactly one x402 workload must appear in mainnet mix");
        assert_eq!(
            x402[0].weight, 22,
            "preliminary measured share must receive exactly 22 of 1000 weight units"
        );
        assert_eq!(setup.pair_token.token, Some(USDC), "existing token setup must fund real USDC");
        assert_eq!(
            setup.pair_token.amount_per_sender,
            U256::from(10_000_000),
            "each payer must start with at least ten USDC"
        );
    }

    /// Settlement calldata must carry a payer signature, not the facilitator's signature.
    #[test]
    fn settlement_recovers_payer_and_encodes_usdc_call() {
        let payer = PrivateKeySigner::from_slice(&[1; 32]).expect("payer key must parse");
        let facilitator = address!("1111111111111111111111111111111111111111");
        let merchant = address!("2222222222222222222222222222222222222222");
        let domain = X402Context::domain(8453);
        assert_eq!(
            domain.hash_struct(),
            b256!("02fa7265e7c5d81118673727957699e4d68f74cd74b7db77da710fe8a2c7834f"),
            "domain must match public Base USDC DOMAIN_SEPARATOR"
        );
        let context = X402Context::new(8453, 1_800_000_000, vec![(facilitator, payer.clone())]);
        let payload = X402Payload::new(context);
        let request = payload.generate(&mut SeededRng::new(7), facilitator, merchant);
        assert_eq!(request.to, Some(USDC.into()), "settlement must target real Base USDC");
        let input = request.input.input().expect("settlement must contain calldata");
        assert_eq!(input.len(), 292, "nine ABI words plus selector must be encoded");
        let call = transferWithAuthorizationCall::abi_decode(input).expect("USDC call must decode");
        assert_eq!(call.from, payer.address(), "authorization owner must be payer");
        assert_eq!(call.to, merchant, "merchant must receive settlement");
        assert_eq!(call.value, U256::from(1), "each settlement must transfer one micro-USDC");
        assert_eq!(
            call.validAfter,
            U256::from(1_799_999_999),
            "authorization must start before snapshot timestamp"
        );
        assert_eq!(
            call.validBefore,
            U256::from(1_800_604_800),
            "authorization must expire after seven snapshot days"
        );
        let authorization = TransferWithAuthorization {
            from: call.from,
            to: call.to,
            value: call.value,
            validAfter: call.validAfter,
            validBefore: call.validBefore,
            nonce: call.nonce,
        };
        let signature =
            alloy_primitives::Signature::from_scalars_and_parity(call.r, call.s, call.v == 28);
        assert_eq!(
            signature
                .recover_address_from_prehash(&authorization.eip712_signing_hash(&domain))
                .expect("signature must recover"),
            payer.address(),
            "payer must authorize the exact USDC domain and fields"
        );
        assert_ne!(
            payer.address(),
            facilitator,
            "facilitator must be a distinct gas-paying account"
        );
    }

    /// Reconstructing a generator with the same seed must never repeat an authorization nonce.
    #[test]
    fn reconstructed_generators_do_not_reuse_authorizations() {
        let payer = PrivateKeySigner::from_slice(&[1; 32]).expect("payer key must parse");
        let facilitator = address!("1111111111111111111111111111111111111111");
        let context = X402Context::new(8453, 1_800_000_000, vec![(facilitator, payer)]);
        let transactions = [crate::runner::TxConfig {
            weight: 100,
            tx_type: crate::runner::TxType::X402 { context: Arc::new(OnceLock::from(context)) },
        }];
        let mut nonces = std::collections::HashSet::new();
        for _ in 0..100 {
            let mut generator = crate::workload::WorkloadGenerator::from_tx_configs(
                crate::config::WorkloadConfig::new("calibration").with_seed(7),
                &transactions,
                None,
            )
            .expect("reconstructed generator must install x402");
            let request = generator
                .generate_payload(facilitator, Address::repeat_byte(2))
                .expect("prepared generator must produce a settlement");
            let call = transferWithAuthorizationCall::abi_decode(
                request.input.input().expect("calldata must exist"),
            )
            .expect("calldata must decode");
            assert!(
                nonces.insert(call.nonce),
                "reconstruction must not reuse a signed authorization nonce"
            );
        }
        assert_eq!(nonces.len(), 100, "all 100 authorizations must have distinct nonces");
    }
}
