//! `OutputProposer` trait and implementations for L1 transaction submission.
//!
//! Submits output proposals by creating new dispute games via `DisputeGameFactory.create()`.
//!
//! Supports two signing modes:
//! - **Local**: Signs with an in-process private key via [`EthereumWallet`].
//! - **Remote**: Calls a signer sidecar's `eth_signTransaction` JSON-RPC method.

use std::future::Future;
use std::sync::Arc;

use alloy::primitives::{Address, Bytes, U256};
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
use crate::constants::{
    ECDSA_SIGNATURE_LENGTH, ECDSA_V_OFFSET, GAS_LIMIT_MULTIPLIER_DENOMINATOR,
    GAS_LIMIT_MULTIPLIER_NUMERATOR, PROOF_TYPE_TEE,
};
use crate::contracts::dispute_game_factory::{encode_create_calldata, encode_extra_data};
use crate::prover::ProverProposal;

/// Applies a 120% safety margin to a gas estimate using integer arithmetic.
const fn apply_gas_margin(estimated: u64) -> u64 {
    estimated.saturating_mul(GAS_LIMIT_MULTIPLIER_NUMERATOR) / GAS_LIMIT_MULTIPLIER_DENOMINATOR
}

/// Builds the proof data for `AggregateVerifier.initialize()`.
///
/// Format: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)` = 130 bytes.
///
/// Matches Go's `buildProofData()` in `driver.go`.
pub fn build_proof_data(proposal: &ProverProposal) -> Result<Bytes, ProposerError> {
    let sig = &proposal.output.signature;
    if sig.len() < ECDSA_SIGNATURE_LENGTH {
        return Err(ProposerError::Internal(format!(
            "signature too short: expected at least {ECDSA_SIGNATURE_LENGTH} bytes, got {}",
            sig.len()
        )));
    }

    let mut proof_data = vec![0u8; 1 + 32 + 32 + ECDSA_SIGNATURE_LENGTH];

    // Byte 0: proof type (TEE = 0)
    proof_data[0] = PROOF_TYPE_TEE;

    // Bytes 1-32: L1 origin hash
    proof_data[1..33].copy_from_slice(proposal.to.l1origin.hash.as_slice());

    // Bytes 33-64: L1 origin number as 32-byte big-endian uint256
    // The uint64 is placed in the last 8 bytes of the 32-byte field (bytes 57-64)
    proof_data[57..65].copy_from_slice(&proposal.to.l1origin.number.to_be_bytes());

    // Bytes 65-129: ECDSA signature with v-value adjusted from 0/1 to 27/28
    proof_data[65..130].copy_from_slice(&sig[..ECDSA_SIGNATURE_LENGTH]);
    if proof_data[129] < ECDSA_V_OFFSET {
        proof_data[129] += ECDSA_V_OFFSET;
    }

    Ok(Bytes::from(proof_data))
}

/// Shared logic for building, signing, broadcasting, and confirming a proposal transaction.
///
/// The `sign_tx` closure parameterizes the signing step so that both local and
/// remote signing modes can reuse the same transaction-building code.
async fn submit_proposal<F, Fut>(
    provider: &RootProvider,
    from_address: Address,
    factory_address: Address,
    calldata: Bytes,
    init_bond: U256,
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

    let mut tx = alloy::rpc::types::TransactionRequest::default()
        .from(from_address)
        .to(factory_address)
        .input(alloy::rpc::types::TransactionInput::new(calldata))
        .nonce(nonce)
        .value(init_bond)
        .max_fee_per_gas(fees.max_fee_per_gas)
        .max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    tx.set_chain_id(chain_id);

    let gas_estimate = provider
        .estimate_gas(tx.clone())
        .await
        .map_err(|e| ProposerError::Contract(format!("estimate_gas failed: {e}")))?;

    tx.set_gas_limit(apply_gas_margin(gas_estimate));

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

/// Returns true if the error is retryable (not a revert, not `GameAlreadyExists`).
fn is_retryable(e: &ProposerError) -> bool {
    if let ProposerError::Contract(msg) = e {
        if msg.contains("reverted") || msg.contains("GameAlreadyExists") {
            return false;
        }
    }
    true
}

/// Returns true if the error indicates the game already exists.
pub fn is_game_already_exists(e: &ProposerError) -> bool {
    matches!(e, ProposerError::Contract(msg) if msg.contains("GameAlreadyExists"))
}

/// Trait for submitting output proposals to L1 via dispute game creation.
#[async_trait]
pub trait OutputProposer: Send + Sync {
    /// Creates a new dispute game for the given proposal.
    ///
    /// Returns the result of the transaction. The caller should query
    /// factory `gameCount()` afterwards to get the new game's factory index.
    async fn propose_output(
        &self,
        proposal: &ProverProposal,
        parent_index: u32,
    ) -> Result<(), ProposerError>;
}

/// Output proposer that signs transactions locally with a private key.
pub struct LocalOutputProposer {
    provider: RootProvider,
    wallet: EthereumWallet,
    factory_address: Address,
    game_type: u32,
    init_bond: U256,
    retry_config: RetryConfig,
    chain_id: OnceCell<u64>,
}

impl std::fmt::Debug for LocalOutputProposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOutputProposer")
            .field("factory_address", &self.factory_address)
            .field("game_type", &self.game_type)
            .finish_non_exhaustive()
    }
}

impl LocalOutputProposer {
    /// Creates a new local output proposer with the given signer.
    pub fn new(
        l1_rpc_url: Url,
        factory_address: Address,
        game_type: u32,
        init_bond: U256,
        signer: PrivateKeySigner,
        retry_config: RetryConfig,
    ) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let wallet = EthereumWallet::from(signer);

        Self {
            provider,
            wallet,
            factory_address,
            game_type,
            init_bond,
            retry_config,
            chain_id: OnceCell::new(),
        }
    }
}

#[async_trait]
impl OutputProposer for LocalOutputProposer {
    async fn propose_output(
        &self,
        proposal: &ProverProposal,
        parent_index: u32,
    ) -> Result<(), ProposerError> {
        let proof_data = build_proof_data(proposal)?;
        let extra_data = encode_extra_data(proposal.to.number, parent_index);
        let calldata = encode_create_calldata(
            self.game_type,
            proposal.output.output_root,
            extra_data,
            proof_data,
        );
        let l2_block_number = proposal.to.number;
        let factory_address = self.factory_address;
        let from = alloy_network::NetworkWallet::<alloy_network::Ethereum>::default_signer_address(
            &self.wallet,
        );

        info!(
            l2_block_number,
            factory = %factory_address,
            game_type = self.game_type,
            parent_index,
            "Creating dispute game via local signer"
        );

        (|| async {
            submit_proposal(
                &self.provider,
                from,
                factory_address,
                calldata.clone(),
                self.init_bond,
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
    factory_address: Address,
    game_type: u32,
    init_bond: U256,
    retry_config: RetryConfig,
    chain_id: OnceCell<u64>,
}

impl std::fmt::Debug for RemoteOutputProposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteOutputProposer")
            .field("signer_address", &self.signer_address)
            .field("factory_address", &self.factory_address)
            .field("game_type", &self.game_type)
            .finish_non_exhaustive()
    }
}

impl RemoteOutputProposer {
    /// Creates a new remote output proposer.
    pub fn new(
        l1_rpc_url: Url,
        factory_address: Address,
        game_type: u32,
        init_bond: U256,
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
            factory_address,
            game_type,
            init_bond,
            retry_config,
            chain_id: OnceCell::new(),
        })
    }
}

#[async_trait]
impl OutputProposer for RemoteOutputProposer {
    async fn propose_output(
        &self,
        proposal: &ProverProposal,
        parent_index: u32,
    ) -> Result<(), ProposerError> {
        use jsonrpsee::core::client::ClientT;
        use jsonrpsee::core::params::ArrayParams;

        let proof_data = build_proof_data(proposal)?;
        let extra_data = encode_extra_data(proposal.to.number, parent_index);
        let calldata = encode_create_calldata(
            self.game_type,
            proposal.output.output_root,
            extra_data,
            proof_data,
        );
        let l2_block_number = proposal.to.number;
        let factory_address = self.factory_address;

        info!(
            l2_block_number,
            factory = %factory_address,
            game_type = self.game_type,
            parent_index,
            signer = %self.signer_address,
            "Creating dispute game via remote signer"
        );

        (|| async {
            submit_proposal(
                &self.provider,
                self.signer_address,
                factory_address,
                calldata.clone(),
                self.init_bond,
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
    factory_address: Address,
    game_type: u32,
    init_bond: U256,
    signing_config: SigningConfig,
    retry_config: RetryConfig,
) -> Result<Arc<dyn OutputProposer>, ProposerError> {
    match signing_config {
        SigningConfig::Local { signer } => {
            let proposer = LocalOutputProposer::new(
                l1_rpc_url,
                factory_address,
                game_type,
                init_bond,
                signer,
                retry_config,
            );
            Ok(Arc::new(proposer))
        }
        SigningConfig::Remote { endpoint, address } => {
            let proposer = RemoteOutputProposer::new(
                l1_rpc_url,
                factory_address,
                game_type,
                init_bond,
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
        let result = apply_gas_margin(u64::MAX);
        assert_eq!(result, u64::MAX / GAS_LIMIT_MULTIPLIER_DENOMINATOR);
    }

    // ========================================================================
    // Proof data encoding tests
    // ========================================================================

    #[test]
    fn test_build_proof_data_length() {
        let proposal = test_proposal(101, 200, false);
        let proof = build_proof_data(&proposal).unwrap();
        // 1 (type) + 32 (l1OriginHash) + 32 (l1OriginNumber) + 65 (sig) = 130
        assert_eq!(proof.len(), 130);
    }

    #[test]
    fn test_build_proof_data_type_byte() {
        let proposal = test_proposal(101, 200, false);
        let proof = build_proof_data(&proposal).unwrap();
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[test]
    fn test_build_proof_data_v_value_adjustment() {
        let mut proposal = test_proposal(101, 200, false);
        // Set v=0 in signature (last byte)
        let mut sig = proposal.output.signature.to_vec();
        sig[64] = 0;
        proposal.output.signature = Bytes::from(sig);

        let proof = build_proof_data(&proposal).unwrap();
        // v should be adjusted to 27
        assert_eq!(proof[129], 27);
    }

    #[test]
    fn test_build_proof_data_v_value_already_adjusted() {
        let mut proposal = test_proposal(101, 200, false);
        // Set v=28 in signature (already adjusted)
        let mut sig = proposal.output.signature.to_vec();
        sig[64] = 28;
        proposal.output.signature = Bytes::from(sig);

        let proof = build_proof_data(&proposal).unwrap();
        // v should remain 28
        assert_eq!(proof[129], 28);
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
    fn test_retry_predicate_skips_game_already_exists() {
        let e = ProposerError::Contract("GameAlreadyExists: 0x123".into());
        assert!(!is_retryable(&e));
    }

    #[test]
    fn test_is_game_already_exists() {
        let e = ProposerError::Contract("GameAlreadyExists: 0xabc".into());
        assert!(is_game_already_exists(&e));

        let e = ProposerError::Contract("some other error".into());
        assert!(!is_game_already_exists(&e));
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
            1,
            U256::ZERO,
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
            1,
            U256::ZERO,
            SigningConfig::Remote {
                endpoint: Url::parse("http://localhost:8546").unwrap(),
                address: Address::ZERO,
            },
            RetryConfig::default(),
        );
        assert!(result.is_ok());
    }
}
