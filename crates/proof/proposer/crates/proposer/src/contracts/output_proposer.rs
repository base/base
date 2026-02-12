//! `OutputProposer` trait and implementations for L1 transaction submission.
//!
//! Supports two signing modes:
//! - **Local**: Signs with an in-process private key via [`EthereumWallet`].
//! - **Remote**: Calls a signer sidecar's `eth_signTransaction` JSON-RPC method.

use std::future::Future;
use std::sync::Arc;

use alloy::primitives::{Address, Bytes};
use alloy::providers::{Provider, RootProvider};
use alloy::signers::local::PrivateKeySigner;
use alloy_eips::Encodable2718;
use alloy_network::{EthereumWallet, TransactionBuilder};
use async_trait::async_trait;
use backon::Retryable;
use tokio::sync::OnceCell;
use tracing::info;
use url::Url;

use crate::ProposerError;
use crate::config::{RetryConfig, SigningConfig};
use crate::constants::{GAS_LIMIT_MULTIPLIER_DENOMINATOR, GAS_LIMIT_MULTIPLIER_NUMERATOR};
use crate::contracts::onchain_verifier::build_verify_calldata_from_proposal;
use crate::prover::ProverProposal;

/// Applies a 120% safety margin to a gas estimate using integer arithmetic.
const fn apply_gas_margin(estimated: u64) -> u64 {
    estimated.saturating_mul(GAS_LIMIT_MULTIPLIER_NUMERATOR) / GAS_LIMIT_MULTIPLIER_DENOMINATOR
}

/// Shared logic for building, signing, broadcasting, and confirming a proposal transaction.
///
/// The `sign_tx` closure parameterizes the signing step so that both local and
/// remote signing modes can reuse the same transaction-building code.
async fn submit_proposal<F, Fut>(
    provider: &RootProvider,
    from_address: Address,
    verifier_address: Address,
    calldata: Bytes,
    l2_block_number: u64,
    chain_id_cell: &OnceCell<u64>,
    sign_tx: F,
) -> Result<(), ProposerError>
where
    F: FnOnce(alloy::rpc::types::TransactionRequest) -> Fut,
    Fut: Future<Output = Result<Bytes, ProposerError>>,
{
    let nonce = provider
        .get_transaction_count(from_address)
        .await
        .map_err(|e| ProposerError::Contract(format!("get_transaction_count failed: {e}")))?;

    let chain_id = *chain_id_cell
        .get_or_try_init(|| async {
            provider
                .get_chain_id()
                .await
                .map_err(|e| ProposerError::Contract(format!("get_chain_id failed: {e}")))
        })
        .await?;

    let fees = provider
        .estimate_eip1559_fees()
        .await
        .map_err(|e| ProposerError::Contract(format!("estimate_eip1559_fees failed: {e}")))?;

    // Build the transaction
    let mut tx = alloy::rpc::types::TransactionRequest::default()
        .from(from_address)
        .to(verifier_address)
        .input(alloy::rpc::types::TransactionInput::new(calldata))
        .nonce(nonce)
        .max_fee_per_gas(fees.max_fee_per_gas)
        .max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    tx.set_chain_id(chain_id);

    let gas_estimate = provider
        .estimate_gas(tx.clone())
        .await
        .map_err(|e| ProposerError::Contract(format!("estimate_gas failed: {e}")))?;

    tx.set_gas_limit(apply_gas_margin(gas_estimate));

    // Sign and broadcast
    let signed_bytes = sign_tx(tx).await?;
    let pending = provider
        .send_raw_transaction(&signed_bytes)
        .await
        .map_err(|e| ProposerError::Contract(format!("send_raw_transaction failed: {e}")))?;

    let tx_hash = *pending.tx_hash();
    info!(%tx_hash, l2_block_number, "Transaction sent, waiting for receipt");

    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| ProposerError::Contract(format!("get_receipt failed: {e}")))?;

    if !receipt.status() {
        return Err(ProposerError::Contract(format!(
            "transaction {tx_hash} reverted"
        )));
    }

    info!(
        %tx_hash,
        l2_block_number,
        block_number = receipt.block_number,
        "Proposal transaction confirmed"
    );
    Ok(())
}

/// Returns true if the error is retryable (i.e. not a revert).
fn is_retryable(e: &ProposerError) -> bool {
    !matches!(e, ProposerError::Contract(msg) if msg.contains("reverted"))
}

/// Trait for submitting output proposals to L1.
#[async_trait]
pub trait OutputProposer: Send + Sync {
    /// Submits a proposal transaction to L1 and waits for inclusion.
    async fn propose_output(&self, proposal: &ProverProposal) -> Result<(), ProposerError>;
}

/// Output proposer that signs transactions locally with a private key.
pub struct LocalOutputProposer {
    provider: RootProvider,
    wallet: EthereumWallet,
    verifier_address: Address,
    retry_config: RetryConfig,
    chain_id: OnceCell<u64>,
}

impl std::fmt::Debug for LocalOutputProposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOutputProposer")
            .field("verifier_address", &self.verifier_address)
            .finish_non_exhaustive()
    }
}

impl LocalOutputProposer {
    /// Creates a new local output proposer with the given signer.
    pub fn new(
        l1_rpc_url: Url,
        verifier_address: Address,
        signer: PrivateKeySigner,
        retry_config: RetryConfig,
    ) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let wallet = EthereumWallet::from(signer);

        Self {
            provider,
            wallet,
            verifier_address,
            retry_config,
            chain_id: OnceCell::new(),
        }
    }
}

#[async_trait]
impl OutputProposer for LocalOutputProposer {
    async fn propose_output(&self, proposal: &ProverProposal) -> Result<(), ProposerError> {
        let calldata = build_verify_calldata_from_proposal(proposal)?;
        let l2_block_number = proposal.to.number;
        let verifier_address = self.verifier_address;
        let from = alloy_network::NetworkWallet::<alloy_network::Ethereum>::default_signer_address(
            &self.wallet,
        );

        info!(
            l2_block_number,
            verifier = %verifier_address,
            "Submitting proposal via local signer"
        );

        (|| async {
            submit_proposal(
                &self.provider,
                from,
                verifier_address,
                calldata.clone(),
                l2_block_number,
                &self.chain_id,
                |tx| async {
                    let envelope = <alloy::rpc::types::TransactionRequest as TransactionBuilder<
                        alloy_network::Ethereum,
                    >>::build(tx, &self.wallet)
                    .await
                    .map_err(|e| {
                        ProposerError::Contract(format!("sign_transaction failed: {e}"))
                    })?;
                    Ok(Bytes::from(Encodable2718::encoded_2718(&envelope)))
                },
            )
            .await
        })
        .retry(self.retry_config.to_backoff_builder())
        .when(is_retryable)
        .await
    }
}

/// Output proposer that signs transactions via a remote signer sidecar.
pub struct RemoteOutputProposer {
    provider: RootProvider,
    signer_client: jsonrpsee::http_client::HttpClient,
    signer_address: Address,
    verifier_address: Address,
    retry_config: RetryConfig,
    chain_id: OnceCell<u64>,
}

impl std::fmt::Debug for RemoteOutputProposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteOutputProposer")
            .field("signer_address", &self.signer_address)
            .field("verifier_address", &self.verifier_address)
            .finish_non_exhaustive()
    }
}

impl RemoteOutputProposer {
    /// Creates a new remote output proposer.
    pub fn new(
        l1_rpc_url: Url,
        verifier_address: Address,
        signer_endpoint: Url,
        signer_address: Address,
        retry_config: RetryConfig,
    ) -> Result<Self, ProposerError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let signer_client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(signer_endpoint.as_str())
            .map_err(|e| ProposerError::Config(format!("failed to build signer client: {e}")))?;

        Ok(Self {
            provider,
            signer_client,
            signer_address,
            verifier_address,
            retry_config,
            chain_id: OnceCell::new(),
        })
    }
}

#[async_trait]
impl OutputProposer for RemoteOutputProposer {
    async fn propose_output(&self, proposal: &ProverProposal) -> Result<(), ProposerError> {
        use jsonrpsee::core::client::ClientT;
        use jsonrpsee::core::params::ArrayParams;

        let calldata = build_verify_calldata_from_proposal(proposal)?;
        let l2_block_number = proposal.to.number;
        let verifier_address = self.verifier_address;

        info!(
            l2_block_number,
            verifier = %verifier_address,
            signer = %self.signer_address,
            "Submitting proposal via remote signer"
        );

        (|| async {
            submit_proposal(
                &self.provider,
                self.signer_address,
                verifier_address,
                calldata.clone(),
                l2_block_number,
                &self.chain_id,
                |tx| async move {
                    let mut params = ArrayParams::new();
                    params.insert(&tx).map_err(|e| {
                        ProposerError::Contract(format!("failed to serialize tx: {e}"))
                    })?;

                    let signed: Bytes = self
                        .signer_client
                        .request("eth_signTransaction", params)
                        .await
                        .map_err(|e| {
                            ProposerError::Contract(format!("eth_signTransaction failed: {e}"))
                        })?;
                    Ok(signed)
                },
            )
            .await
        })
        .retry(self.retry_config.to_backoff_builder())
        .when(is_retryable)
        .await
    }
}

/// Creates an [`OutputProposer`] based on the signing configuration.
pub fn create_output_proposer(
    l1_rpc_url: Url,
    verifier_address: Address,
    signing_config: SigningConfig,
    retry_config: RetryConfig,
) -> Result<Arc<dyn OutputProposer>, ProposerError> {
    match signing_config {
        SigningConfig::Local { signer } => {
            let proposer =
                LocalOutputProposer::new(l1_rpc_url, verifier_address, signer, retry_config);
            Ok(Arc::new(proposer))
        }
        SigningConfig::Remote { endpoint, address } => {
            let proposer = RemoteOutputProposer::new(
                l1_rpc_url,
                verifier_address,
                endpoint,
                address,
                retry_config,
            )?;
            Ok(Arc::new(proposer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, U256};
    use alloy_sol_types::SolCall;

    use crate::contracts::onchain_verifier::{
        IOnchainVerifier, build_verify_calldata, decode_proof_bytes,
    };
    use crate::prover::types::test_helpers::test_proposal;

    // ========================================================================
    // Gas margin tests
    // ========================================================================

    #[test]
    fn test_apply_gas_margin_typical() {
        assert_eq!(apply_gas_margin(100_000), 120_000);
    }

    #[test]
    fn test_apply_gas_margin_zero() {
        assert_eq!(apply_gas_margin(0), 0);
    }

    #[test]
    fn test_apply_gas_margin_overflow_saturates() {
        // u64::MAX * 6 would overflow; should saturate to u64::MAX / 5
        let result = apply_gas_margin(u64::MAX);
        assert_eq!(result, u64::MAX / GAS_LIMIT_MULTIPLIER_DENOMINATOR);
    }

    #[test]
    fn test_apply_gas_margin_rounding() {
        // 1 * 6 / 5 = 1 (integer truncation)
        assert_eq!(apply_gas_margin(1), 1);
        // 2 * 6 / 5 = 2
        assert_eq!(apply_gas_margin(2), 2);
        // 3 * 6 / 5 = 3
        assert_eq!(apply_gas_margin(3), 3);
        // 4 * 6 / 5 = 4
        assert_eq!(apply_gas_margin(4), 4);
        // 5 * 6 / 5 = 6
        assert_eq!(apply_gas_margin(5), 6);
    }

    // ========================================================================
    // Calldata round-trip tests
    // ========================================================================

    #[test]
    fn test_build_verify_calldata_round_trip() {
        let l1_origin_number: u64 = 42;
        let l1_origin_hash = B256::repeat_byte(0xAA);
        let prev_output_root = B256::repeat_byte(0xBB);
        let config_hash = B256::repeat_byte(0xCC);
        let output_root = B256::repeat_byte(0xDD);
        let l2_block_number = U256::from(999);
        let signature = vec![0x11; 65];

        let calldata = build_verify_calldata(
            l1_origin_number,
            l1_origin_hash,
            prev_output_root,
            config_hash,
            &signature,
            output_root,
            l2_block_number,
        )
        .unwrap();

        // Decode the outer verify call
        let decoded = IOnchainVerifier::verifyCall::abi_decode(&calldata).unwrap();
        assert_eq!(decoded.rootClaim, output_root);
        assert_eq!(decoded.l2BlockNumber, l2_block_number);

        // Decode the inner tuple: (uint256, bytes32, bytes32, bytes32, bytes)
        type InnerTuple = (
            alloy_sol_types::sol_data::Uint<256>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::Bytes,
        );
        let (decoded_l1_num, decoded_l1_hash, decoded_prev, decoded_cfg, decoded_proof) =
            <InnerTuple as alloy_sol_types::SolType>::abi_decode_params(&decoded.proofBytes)
                .unwrap();

        assert_eq!(decoded_l1_num, U256::from(l1_origin_number));
        assert_eq!(B256::from(decoded_l1_hash), l1_origin_hash);
        assert_eq!(B256::from(decoded_prev), prev_output_root);
        assert_eq!(B256::from(decoded_cfg), config_hash);
        // proof bytes should be 65 bytes with v adjusted
        assert_eq!(decoded_proof.len(), 65);
    }

    #[test]
    fn test_build_verify_calldata_v_value_round_trip() {
        let mut signature = vec![0x00; 65];
        signature[64] = 1; // v = 1

        let calldata = build_verify_calldata(
            100,
            B256::ZERO,
            B256::ZERO,
            B256::ZERO,
            &signature,
            B256::ZERO,
            U256::from(200),
        )
        .unwrap();

        // Decode the outer call
        let decoded = IOnchainVerifier::verifyCall::abi_decode(&calldata).unwrap();

        // Decode inner tuple to get proof bytes
        type InnerTuple = (
            alloy_sol_types::sol_data::Uint<256>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::Bytes,
        );
        let (_, _, _, _, encoded_proof) =
            <InnerTuple as alloy_sol_types::SolType>::abi_decode_params(&decoded.proofBytes)
                .unwrap();

        // The encoded proof should have v = 28 (1 + 27)
        assert_eq!(encoded_proof[64], 28);

        // decode_proof_bytes should bring it back to v = 1
        let round_tripped = decode_proof_bytes(&encoded_proof);
        assert_eq!(round_tripped[64], 1);
    }

    #[test]
    fn test_build_verify_calldata_boundary_values() {
        let signature = vec![0xFF; 65];

        let calldata = build_verify_calldata(
            u64::MAX,
            B256::ZERO,
            B256::ZERO,
            B256::ZERO,
            &signature,
            B256::ZERO,
            U256::MAX,
        )
        .unwrap();

        let decoded = IOnchainVerifier::verifyCall::abi_decode(&calldata).unwrap();
        assert_eq!(decoded.l2BlockNumber, U256::MAX);

        type InnerTuple = (
            alloy_sol_types::sol_data::Uint<256>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::FixedBytes<32>,
            alloy_sol_types::sol_data::Bytes,
        );
        let (decoded_l1_num, _, _, _, _) =
            <InnerTuple as alloy_sol_types::SolType>::abi_decode_params(&decoded.proofBytes)
                .unwrap();
        assert_eq!(decoded_l1_num, U256::from(u64::MAX));
    }

    // ========================================================================
    // Retry predicate tests
    // ========================================================================

    #[test]
    fn test_retry_predicate_retries_transient_errors() {
        let e = ProposerError::Contract("get_transaction_count failed: timeout".into());
        assert!(is_retryable(&e));
    }

    #[test]
    fn test_retry_predicate_skips_reverts() {
        let e = ProposerError::Contract("transaction 0x123 reverted".into());
        assert!(!is_retryable(&e));
    }

    #[test]
    fn test_retry_predicate_retries_non_contract_errors() {
        let e = ProposerError::Internal("something went wrong".into());
        assert!(is_retryable(&e));

        let e = ProposerError::Config("bad config".into());
        assert!(is_retryable(&e));
    }

    // ========================================================================
    // Factory function tests
    // ========================================================================

    #[test]
    fn test_create_output_proposer_local() {
        use alloy::signers::k256::ecdsa::SigningKey;

        let key_bytes =
            hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap();
        let signing_key = SigningKey::from_slice(&key_bytes).unwrap();
        let signer = PrivateKeySigner::from_signing_key(signing_key);

        let result = create_output_proposer(
            Url::parse("http://localhost:8545").unwrap(),
            Address::ZERO,
            SigningConfig::Local { signer },
            RetryConfig::default(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_output_proposer_remote() {
        let result = create_output_proposer(
            Url::parse("http://localhost:8545").unwrap(),
            Address::ZERO,
            SigningConfig::Remote {
                endpoint: Url::parse("http://localhost:8546").unwrap(),
                address: Address::ZERO,
            },
            RetryConfig::default(),
        );
        assert!(result.is_ok());
    }

    // ========================================================================
    // Proposal-to-calldata field mapping
    // ========================================================================

    #[test]
    fn test_build_verify_calldata_from_proposal_fields() {
        let proposal = test_proposal(101, 200, false);
        let calldata = build_verify_calldata_from_proposal(&proposal).unwrap();

        let decoded = IOnchainVerifier::verifyCall::abi_decode(&calldata).unwrap();
        assert_eq!(decoded.rootClaim, proposal.output.output_root);
        assert_eq!(decoded.l2BlockNumber, proposal.output.l2_block_number);
    }
}
