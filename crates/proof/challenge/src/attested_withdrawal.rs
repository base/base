//! Types and encoding helpers for the attested-withdrawal relay.

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types_eth::Filter;
use alloy_sol_types::{SolEvent, SolValue};
use async_trait::async_trait;
use base_common_consensus::Predeploys;
use base_proof_contracts::{
    IL2ToL1MessagePasser, OptimismPortalClient, encode_redeem_attested_withdrawal_calldata,
};
use base_proof_primitives::{ATTESTED_WITHDRAWAL_SLOT, AttestedWithdrawalApiClient};
use base_proof_rpc::L2Provider;
use base_tx_manager::{TxCandidate, TxManager};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use url::Url;

/// Configuration for the optional attested-withdrawal relay.
#[derive(Debug, Clone)]
pub struct AttestedWithdrawalRelayConfig {
    /// L1 `OptimismPortal2` address.
    pub portal_address: Address,
    /// Private enclave JSON-RPC endpoint.
    pub enclave_rpc_url: Url,
    /// First L2 block to scan.
    pub start_block: u64,
    /// Delay between scans.
    pub poll_interval: Duration,
    /// L2 confirmations required before processing a log.
    pub confirmations: u64,
    /// Maximum number of L2 blocks in one log query.
    pub scan_batch_size: u64,
}

/// Requests an enclave signature for a verified attested withdrawal.
#[async_trait]
pub trait AttestedWithdrawalSigner: Send + Sync {
    /// Returns the raw 65-byte ECDSA signature for an authorization hash.
    async fn sign_attested_withdrawal(
        &self,
        auth_hash: B256,
        message_passer_storage_root: B256,
        storage_proof: Vec<Bytes>,
    ) -> Result<Vec<u8>, AttestedWithdrawalRelayError>;
}

#[async_trait]
impl AttestedWithdrawalSigner for HttpClient {
    async fn sign_attested_withdrawal(
        &self,
        auth_hash: B256,
        message_passer_storage_root: B256,
        storage_proof: Vec<Bytes>,
    ) -> Result<Vec<u8>, AttestedWithdrawalRelayError> {
        AttestedWithdrawalApiClient::sign_attested_withdrawal(
            self,
            auth_hash,
            message_passer_storage_root,
            storage_proof,
        )
        .await
        .map_err(|error| AttestedWithdrawalRelayError::Signer(error.to_string()))
    }
}

/// Polls L2 attested-withdrawal logs and redeems them on L1.
///
/// The scan cursor is in memory. A restart rescans from `start_block`; portal
/// replay protection makes that safe while durable progress remains deferred.
#[derive(Debug)]
pub struct AttestedWithdrawalRelayer<L2, S, P, T>
where
    L2: L2Provider,
    S: AttestedWithdrawalSigner,
    P: OptimismPortalClient,
    T: TxManager,
{
    config: AttestedWithdrawalRelayConfig,
    l2_provider: Arc<L2>,
    signer: S,
    portal: P,
    tx_manager: T,
    l2_chain_id: u64,
    next_block: u64,
}

impl<L2, S, P, T> AttestedWithdrawalRelayer<L2, S, P, T>
where
    L2: L2Provider,
    S: AttestedWithdrawalSigner,
    P: OptimismPortalClient,
    T: TxManager,
{
    /// Creates a relayer and reads the L2 chain ID used to validate event hashes.
    pub async fn new(
        config: AttestedWithdrawalRelayConfig,
        l2_provider: Arc<L2>,
        signer: S,
        portal: P,
        tx_manager: T,
    ) -> Result<Self, AttestedWithdrawalRelayError> {
        let l2_chain_id = l2_provider
            .chain_id()
            .await
            .map_err(|error| AttestedWithdrawalRelayError::L2Rpc(error.to_string()))?;
        Ok(Self {
            next_block: config.start_block,
            config,
            l2_provider,
            signer,
            portal,
            tx_manager,
            l2_chain_id,
        })
    }

    /// Runs relay cycles until cancellation.
    pub async fn run(mut self, cancel: CancellationToken) {
        while !cancel.is_cancelled() {
            if let Err(error) = self.step().await {
                tracing::warn!(error = %error, "attested withdrawal relay step failed");
            }
            select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }

    /// Relays one bounded range of finalized L2 blocks.
    pub async fn step(&mut self) -> Result<(), AttestedWithdrawalRelayError> {
        let head = self
            .l2_provider
            .header_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|error| AttestedWithdrawalRelayError::L2Rpc(error.to_string()))?;
        let Some(finalized_head) = finalized_head(head.inner.number, self.config.confirmations)
        else {
            return Ok(());
        };
        if self.next_block > finalized_head {
            return Ok(());
        }
        let end_block = self
            .next_block
            .saturating_add(self.config.scan_batch_size.saturating_sub(1))
            .min(finalized_head);
        let filter = Filter::new()
            .address(Predeploys::L2_TO_L1_MESSAGE_PASSER)
            .event_signature(IL2ToL1MessagePasser::AttestedWithdrawalInitiated::SIGNATURE_HASH)
            .from_block(self.next_block)
            .to_block(end_block);
        let logs = self
            .l2_provider
            .get_logs(filter)
            .await
            .map_err(|error| AttestedWithdrawalRelayError::L2Rpc(error.to_string()))?;
        for log in logs {
            self.relay_log(&log).await?;
        }
        self.next_block = end_block.saturating_add(1);
        Ok(())
    }

    /// Relays one validated L2 event log.
    pub async fn relay_log(
        &self,
        log: &alloy_rpc_types_eth::Log,
    ) -> Result<(), AttestedWithdrawalRelayError> {
        let event = decode_attested_withdrawal_log(log, self.l2_chain_id)?;
        let block_number =
            log.block_number.ok_or(AttestedWithdrawalRelayError::MissingBlockNumber)?;
        let block_hash = log.block_hash.ok_or(AttestedWithdrawalRelayError::MissingBlockHash)?;
        if self
            .portal
            .attest_redeemed(event.auth_hash)
            .await
            .map_err(|error| AttestedWithdrawalRelayError::Portal(error.to_string()))?
        {
            debug!(auth_hash = %event.auth_hash, l2_block = block_number, "attested withdrawal already redeemed");
            return Ok(());
        }
        let slot = attested_withdrawal_storage_slot(event.auth_hash);
        let account_proof = self
            .l2_provider
            .get_storage_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, vec![slot], block_hash)
            .await
            .map_err(|error| AttestedWithdrawalRelayError::L2Rpc(error.to_string()))?;
        let storage_proof = account_proof.storage_proof;
        if storage_proof.len() != 1
            || storage_proof[0].key.as_b256() != slot
            || storage_proof[0].value.is_zero()
        {
            return Err(AttestedWithdrawalRelayError::InvalidStorageProof);
        }
        let signature = normalize_attested_withdrawal_signature(
            self.signer
                .sign_attested_withdrawal(
                    event.auth_hash,
                    account_proof.storage_hash,
                    storage_proof[0].proof.clone(),
                )
                .await?,
        )?;
        if self
            .portal
            .attest_redeemed(event.auth_hash)
            .await
            .map_err(|error| AttestedWithdrawalRelayError::Portal(error.to_string()))?
        {
            debug!(auth_hash = %event.auth_hash, l2_block = block_number, "attested withdrawal redeemed while signing");
            return Ok(());
        }
        let receipt = self
            .tx_manager
            .send(TxCandidate {
                tx_data: encode_redeem_attested_withdrawal_calldata(
                    event.recipient,
                    event.amount,
                    event.nonce,
                    event.data,
                    Bytes::from(signature),
                ),
                to: Some(self.config.portal_address),
                value: U256::ZERO,
                ..Default::default()
            })
            .await
            .map_err(|error| AttestedWithdrawalRelayError::Transaction(error.to_string()))?;
        if !receipt.inner.status() {
            return Err(AttestedWithdrawalRelayError::TransactionReverted {
                tx_hash: receipt.transaction_hash,
            });
        }
        info!(
            auth_hash = %event.auth_hash,
            l2_block = block_number,
            recipient = %event.recipient,
            amount = %event.amount,
            tx_hash = %receipt.transaction_hash,
            "attested withdrawal redeemed"
        );
        Ok(())
    }
}

/// Builds the private enclave RPC signer client.
pub fn attested_withdrawal_signer_client(
    endpoint: &Url,
) -> Result<HttpClient, AttestedWithdrawalRelayError> {
    HttpClientBuilder::default()
        .build(endpoint.as_str())
        .map_err(|error| AttestedWithdrawalRelayError::Signer(error.to_string()))
}

/// Normalizes an ECDSA recovery byte from compact to Ethereum form.
pub fn normalize_attested_withdrawal_signature(
    mut signature: Vec<u8>,
) -> Result<Vec<u8>, AttestedWithdrawalRelayError> {
    if signature.len() != 65 {
        return Err(AttestedWithdrawalRelayError::InvalidSignatureLength(signature.len()));
    }
    signature[64] = match signature[64] {
        0 => 27,
        1 => 28,
        27 | 28 => signature[64],
        value => return Err(AttestedWithdrawalRelayError::InvalidRecoveryId(value)),
    };
    Ok(signature)
}

const fn finalized_head(head: u64, confirmations: u64) -> Option<u64> {
    head.checked_sub(confirmations)
}

/// Computes the authorization hash emitted by `L2ToL1MessagePasser`.
#[must_use]
pub fn attested_withdrawal_auth_hash(
    l2_chain_id: u64,
    recipient: Address,
    amount: U256,
    nonce: U256,
    data: &Bytes,
) -> B256 {
    keccak256(
        (U256::from(l2_chain_id), recipient, Address::ZERO, amount, nonce, data.clone())
            .abi_encode_params(),
    )
}

/// Computes the `attestedWithdrawals` mapping key for an authorization hash.
#[must_use]
pub fn attested_withdrawal_storage_slot(auth_hash: B256) -> B256 {
    let mut encoded = [0_u8; 64];
    encoded[..32].copy_from_slice(auth_hash.as_slice());
    encoded[56..].copy_from_slice(&ATTESTED_WITHDRAWAL_SLOT.to_be_bytes());
    keccak256(encoded)
}

/// Decoded attested-withdrawal event fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestedWithdrawalEvent {
    /// Authorization hash emitted by the L2 message passer.
    pub auth_hash: B256,
    /// L1 recipient.
    pub recipient: Address,
    /// Withdrawal amount in wei.
    pub amount: U256,
    /// Per-message-passer authorization nonce.
    pub nonce: U256,
    /// Calldata executed with the L1 ETH transfer.
    pub data: Bytes,
}

/// Decodes and validates one `AttestedWithdrawalInitiated` log.
pub fn decode_attested_withdrawal_log(
    log: &alloy_rpc_types_eth::Log,
    l2_chain_id: u64,
) -> Result<AttestedWithdrawalEvent, AttestedWithdrawalRelayError> {
    let event = IL2ToL1MessagePasser::AttestedWithdrawalInitiated::decode_log_data(&log.inner.data)
        .map_err(|error| AttestedWithdrawalRelayError::InvalidEvent(error.to_string()))?;
    if event.token != Address::ZERO {
        return Err(AttestedWithdrawalRelayError::UnsupportedToken(event.token));
    }
    let expected = attested_withdrawal_auth_hash(
        l2_chain_id,
        event.recipient,
        event.amount,
        event.nonce,
        &event.data,
    );
    if event.authHash != expected {
        return Err(AttestedWithdrawalRelayError::AuthorizationHashMismatch {
            expected,
            actual: event.authHash,
        });
    }
    Ok(AttestedWithdrawalEvent {
        auth_hash: event.authHash,
        recipient: event.recipient,
        amount: event.amount,
        nonce: event.nonce,
        data: event.data,
    })
}

/// Errors raised while validating a withdrawal relay record.
#[derive(Debug, thiserror::Error)]
pub enum AttestedWithdrawalRelayError {
    /// The event payload was not a valid attested-withdrawal event.
    #[error("invalid attested withdrawal event: {0}")]
    InvalidEvent(String),
    /// The event requested a non-ETH transfer.
    #[error("unsupported attested withdrawal token: {0}")]
    UnsupportedToken(Address),
    /// The event hash does not match its authorization fields.
    #[error("attested withdrawal authorization hash mismatch: expected {expected}, got {actual}")]
    AuthorizationHashMismatch {
        /// Expected hash.
        expected: B256,
        /// Hash emitted by L2.
        actual: B256,
    },
    /// The log did not include an L2 block number.
    #[error("attested withdrawal log is missing its block number")]
    MissingBlockNumber,
    /// The log did not include an L2 block hash.
    #[error("attested withdrawal log is missing its block hash")]
    MissingBlockHash,
    /// The account proof did not contain exactly the requested storage proof.
    #[error("invalid attested withdrawal storage proof")]
    InvalidStorageProof,
    /// The enclave returned a signature with the wrong length.
    #[error("invalid attested withdrawal signature length: {0}")]
    InvalidSignatureLength(usize),
    /// The enclave returned an unsupported ECDSA recovery ID.
    #[error("invalid attested withdrawal signature recovery ID: {0}")]
    InvalidRecoveryId(u8),
    /// An L2 RPC request failed.
    #[error("L2 RPC error: {0}")]
    L2Rpc(String),
    /// An enclave signer request failed.
    #[error("enclave signer error: {0}")]
    Signer(String),
    /// A portal read failed.
    #[error("portal RPC error: {0}")]
    Portal(String),
    /// L1 transaction submission failed.
    #[error("L1 transaction error: {0}")]
    Transaction(String),
    /// The portal redemption transaction reverted.
    #[error("attested withdrawal redemption transaction reverted: {tx_hash}")]
    TransactionReverted {
        /// Hash of the reverted L1 transaction.
        tx_hash: B256,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_primitives::{Log as PrimitiveLog, address, b256};
    use alloy_rpc_types_eth::{EIP1186AccountProofResponse, EIP1186StorageProof, Log};
    use async_trait::async_trait;
    use base_proof_contracts::{ContractError, OptimismPortalClient};

    use super::*;
    use crate::test_utils::{MockL2Provider, SharedMockTxManager, receipt_with_status};

    #[derive(Debug)]
    struct TestSigner {
        signature: Vec<u8>,
    }

    #[async_trait]
    impl AttestedWithdrawalSigner for TestSigner {
        async fn sign_attested_withdrawal(
            &self,
            _auth_hash: B256,
            _message_passer_storage_root: B256,
            _storage_proof: Vec<Bytes>,
        ) -> Result<Vec<u8>, AttestedWithdrawalRelayError> {
            Ok(self.signature.clone())
        }
    }

    #[derive(Debug, Default)]
    struct TestPortal {
        redeemed: Mutex<Vec<B256>>,
    }

    #[async_trait]
    impl OptimismPortalClient for TestPortal {
        async fn attest_redeemed(&self, auth_hash: B256) -> Result<bool, ContractError> {
            Ok(self.redeemed.lock().unwrap().contains(&auth_hash))
        }
    }

    #[test]
    fn authorization_hash_matches_solidity_abi_encoding() {
        assert_eq!(
            attested_withdrawal_auth_hash(
                8453,
                address!("1234567890123456789012345678901234567890"),
                U256::from(42),
                U256::from(7),
                &Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF]),
            ),
            b256!("e1ae53b3b8b5eaf88e0a84b6ef3982200d38eb401ca039436feca873e18de5a3")
        );
    }

    #[test]
    fn storage_slot_encodes_auth_hash_then_mapping_slot() {
        assert_eq!(
            attested_withdrawal_storage_slot(b256!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            b256!("f04c900c69b2687067bbda7f4040b37d36989c18645b1cd121054e319ee3cac8")
        );
    }

    #[test]
    fn normalizes_compact_recovery_ids() {
        let mut signature = vec![0; 65];
        signature[64] = 1;

        assert_eq!(normalize_attested_withdrawal_signature(signature).unwrap()[64], 28);
    }

    #[test]
    fn rejects_invalid_signature_recovery_id() {
        let mut signature = vec![0; 65];
        signature[64] = 2;

        assert!(matches!(
            normalize_attested_withdrawal_signature(signature),
            Err(AttestedWithdrawalRelayError::InvalidRecoveryId(2))
        ));
    }

    #[test]
    fn requires_the_requested_confirmation_depth() {
        assert_eq!(finalized_head(0, 1), None);
        assert_eq!(finalized_head(1, 1), Some(0));
        assert_eq!(finalized_head(2, 1), Some(1));
    }

    #[tokio::test]
    async fn relay_log_submits_eth_plus_calldata_redemption() {
        let recipient = address!("1234567890123456789012345678901234567890");
        let amount = U256::from(42);
        let nonce = U256::from(7);
        let data = Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let chain_id = 8453;
        let auth_hash = attested_withdrawal_auth_hash(chain_id, recipient, amount, nonce, &data);
        let slot = attested_withdrawal_storage_slot(auth_hash);
        let block_hash = B256::repeat_byte(0xAB);
        let log = Log {
            inner: PrimitiveLog {
                address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
                data: IL2ToL1MessagePasser::AttestedWithdrawalInitiated {
                    authHash: auth_hash,
                    recipient,
                    token: Address::ZERO,
                    amount,
                    nonce,
                    data: data.clone(),
                }
                .encode_log_data(),
            },
            block_hash: Some(block_hash),
            block_number: Some(12),
            ..Default::default()
        };
        let mut provider = MockL2Provider::new();
        provider.chain_id = chain_id;
        provider.proofs.insert(
            block_hash,
            EIP1186AccountProofResponse {
                storage_hash: B256::repeat_byte(0xCD),
                storage_proof: vec![EIP1186StorageProof {
                    key: slot.into(),
                    value: U256::ONE,
                    proof: vec![Bytes::from_static(&[1, 2, 3])],
                }],
                ..Default::default()
            },
        );
        let tx_manager = SharedMockTxManager::with_responses(vec![Ok(receipt_with_status(
            true,
            B256::repeat_byte(0xEF),
        ))]);
        let relayer = AttestedWithdrawalRelayer::new(
            AttestedWithdrawalRelayConfig {
                portal_address: Address::repeat_byte(0x11),
                enclave_rpc_url: Url::parse("http://localhost:9000").unwrap(),
                start_block: 0,
                poll_interval: Duration::from_secs(1),
                confirmations: 0,
                scan_batch_size: 1,
            },
            Arc::new(provider),
            TestSigner { signature: vec![0; 64].into_iter().chain([1]).collect() },
            TestPortal::default(),
            tx_manager.clone(),
        )
        .await
        .unwrap();

        relayer.relay_log(&log).await.unwrap();

        let calls = tx_manager.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(Address::repeat_byte(0x11)));
        assert_eq!(
            calls[0].tx_data,
            encode_redeem_attested_withdrawal_calldata(
                recipient,
                amount,
                nonce,
                data,
                Bytes::from(vec![0; 64].into_iter().chain([28]).collect::<Vec<_>>()),
            )
        );
        assert_eq!(calls[0].value, U256::ZERO);
    }
}
