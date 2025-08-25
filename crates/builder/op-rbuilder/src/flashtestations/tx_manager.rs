use alloy_consensus::TxEip1559;
use alloy_eips::Encodable2718;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256, keccak256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_transport::TransportResult;
use op_alloy_consensus::OpTypedTransaction;
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::Recovered;
use std::time::Duration;

use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue, sol};
use op_alloy_network::Optimism;
use tracing::{debug, error, info};

use crate::tx_signer::Signer;

sol!(
    #[sol(rpc, abi)]
    interface IFlashtestationRegistry {
        function registerTEEService(bytes calldata rawQuote) external;
    }

    #[sol(rpc, abi)]
    interface IBlockBuilderPolicy {
        function verifyBlockBuilderProof(uint8 version, bytes32 blockContentHash) external;
    }

    struct BlockData {
        bytes32 parentHash;
        uint256 blockNumber;
        uint256 timestamp;
        bytes32[] transactionHashes;
    }
);

#[derive(Debug, Clone)]
pub struct TxManager {
    tee_service_signer: Signer,
    funding_signer: Signer,
    rpc_url: String,
    registry_address: Address,
    builder_policy_address: Address,
    builder_proof_version: u8,
}

impl TxManager {
    pub fn new(
        tee_service_signer: Signer,
        funding_signer: Signer,
        rpc_url: String,
        registry_address: Address,
        builder_policy_address: Address,
        builder_proof_version: u8,
    ) -> Self {
        Self {
            tee_service_signer,
            funding_signer,
            rpc_url,
            registry_address,
            builder_policy_address,
            builder_proof_version,
        }
    }

    pub async fn fund_address(&self, from: Signer, to: Address, amount: U256) -> eyre::Result<()> {
        let funding_wallet = PrivateKeySigner::from_bytes(&from.secret.secret_bytes().into())?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .fetch_chain_id()
            .with_gas_estimation()
            .with_cached_nonce_management()
            .wallet(funding_wallet)
            .network::<Optimism>()
            .connect(self.rpc_url.as_str())
            .await?;

        // Create funding transaction
        let funding_tx = TransactionRequest {
            from: Some(from.address),
            to: Some(TxKind::Call(to)),
            value: Some(amount),
            gas: Some(21_000), // Standard gas for ETH transfer
            ..Default::default()
        };

        // Send funding transaction
        match Self::process_pending_tx(provider.send_transaction(funding_tx.into()).await).await {
            Ok(tx_hash) => {
                info!(target: "flashtestations", tx_hash = %tx_hash, "funding transaction confirmed successfully");
            }
            Err(e) => {
                error!(target: "flashtestations", error = %e, "funding transaction failed");
                return Err(e);
            }
        }

        Ok(())
    }

    pub async fn fund_and_register_tee_service(
        &self,
        attestation: Vec<u8>,
        funding_amount: U256,
    ) -> eyre::Result<()> {
        info!(target: "flashtestations", "funding TEE address at {}", self.tee_service_signer.address);
        self.fund_address(
            self.funding_signer,
            self.tee_service_signer.address,
            funding_amount,
        )
        .await?;

        let quote_bytes = Bytes::from(attestation);
        let wallet =
            PrivateKeySigner::from_bytes(&self.tee_service_signer.secret.secret_bytes().into())?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .fetch_chain_id()
            .with_gas_estimation()
            .wallet(wallet)
            .network::<Optimism>()
            .connect(self.rpc_url.as_str())
            .await?;

        info!(target: "flashtestations", "submitting quote to registry at {}", self.registry_address);

        // TODO: add retries
        let calldata = IFlashtestationRegistry::registerTEEServiceCall {
            rawQuote: quote_bytes,
        }
        .abi_encode();
        let tx = TransactionRequest {
            from: Some(self.tee_service_signer.address),
            to: Some(TxKind::Call(self.registry_address)),
            // gas: Some(10_000_000), // Set gas limit manually as the contract is gas heavy
            nonce: Some(0),
            input: calldata.into(),
            ..Default::default()
        };
        match Self::process_pending_tx(provider.send_transaction(tx.into()).await).await {
            Ok(tx_hash) => {
                info!(target: "flashtestations", tx_hash = %tx_hash, "attestation transaction confirmed successfully");
                Ok(())
            }
            Err(e) => {
                error!(target: "flashtestations", error = %e, "attestation transaction failed to be sent");
                Err(e)
            }
        }
    }

    pub fn signed_block_builder_proof(
        &self,
        payload: OpBuiltPayload,
        gas_limit: u64,
        base_fee: u64,
        chain_id: u64,
        nonce: u64,
    ) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error> {
        let block_content_hash = Self::compute_block_content_hash(payload);

        info!(target: "flashtestations",  block_content_hash = ?block_content_hash, "submitting block builder proof transaction");
        let calldata = IBlockBuilderPolicy::verifyBlockBuilderProofCall {
            version: self.builder_proof_version,
            blockContentHash: block_content_hash,
        }
        .abi_encode();
        // Create the EIP-1559 transaction
        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas: base_fee.into(),
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(self.builder_policy_address),
            input: calldata.into(),
            ..Default::default()
        });
        self.tee_service_signer.sign_tx(tx)
    }

    pub async fn clean_up(&self) -> eyre::Result<()> {
        info!(target: "flashtestations", "sending funds back from TEE generated key to funding address");
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Optimism>()
            .connect(self.rpc_url.as_str())
            .await?;
        let balance = provider
            .get_balance(self.tee_service_signer.address)
            .await?;
        let gas_estimate = 21_000u128;
        let gas_price = provider.get_gas_price().await?;
        let gas_cost = U256::from(gas_estimate * gas_price);
        if balance.gt(&gas_cost) {
            self.fund_address(
                self.tee_service_signer,
                self.funding_signer.address,
                balance.saturating_sub(gas_cost),
            )
            .await?
        }
        Ok(())
    }

    /// Processes a pending transaction and logs whether the transaction succeeded or not
    async fn process_pending_tx(
        pending_tx_result: TransportResult<PendingTransactionBuilder<Optimism>>,
    ) -> eyre::Result<TxHash> {
        match pending_tx_result {
            Ok(pending_tx) => {
                let tx_hash = *pending_tx.tx_hash();
                debug!(target: "flashtestations", tx_hash = %tx_hash, "transaction submitted");

                // Wait for funding transaction confirmation
                match pending_tx
                    .with_timeout(Some(Duration::from_secs(30)))
                    .get_receipt()
                    .await
                {
                    Ok(receipt) => {
                        if receipt.status() {
                            Ok(receipt.transaction_hash())
                        } else {
                            Err(eyre::eyre!("Transaction reverted: {}", tx_hash))
                        }
                    }
                    Err(e) => Err(e.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Computes the block content hash according to the formula:
    /// keccak256(abi.encode(parentHash, blockNumber, timestamp, transactionHashes))
    fn compute_block_content_hash(payload: OpBuiltPayload) -> B256 {
        let block = payload.block();
        let body = block.clone().into_body();
        let transactions = body.transactions();

        // Create ordered list of transaction hashes
        let transaction_hashes: Vec<B256> = transactions
            .map(|tx| {
                // RLP encode the transaction and hash it
                let mut encoded = Vec::new();
                tx.encode_2718(&mut encoded);
                keccak256(&encoded)
            })
            .collect();

        // Create struct and ABI encode
        let block_data = BlockData {
            parentHash: block.parent_hash,
            blockNumber: U256::from(block.number),
            timestamp: U256::from(block.timestamp),
            transactionHashes: transaction_hashes,
        };

        let encoded = block_data.abi_encode();
        keccak256(&encoded)
    }
}
