use alloy_json_rpc::RpcError;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::SolCall;
use alloy_transport::{TransportError, TransportErrorKind, TransportResult};
use k256::ecdsa;
use std::time::Duration;

use alloy_provider::{
    PendingTransactionBuilder, PendingTransactionError, Provider, ProviderBuilder,
};
use alloy_signer_local::PrivateKeySigner;
use op_alloy_network::Optimism;
use tracing::{debug, info, warn};

use crate::{flashtestations::IFlashtestationRegistry, tx_signer::Signer};

#[derive(Debug, thiserror::Error)]
pub enum TxManagerError {
    #[error("rpc error: {0}")]
    RpcError(#[from] TransportError),
    #[error("tx reverted: {0}")]
    TxReverted(TxHash),
    #[error("error checking tx confirmation: {0}")]
    TxConfirmationError(PendingTransactionError),
    #[error("tx rpc error: {0}")]
    TxRpcError(RpcError<TransportErrorKind>),
    #[error("signer error: {0}")]
    SignerError(ecdsa::Error),
}

#[derive(Debug, Clone)]
pub struct TxManager {
    tee_service_signer: Signer,
    funding_signer: Signer,
    rpc_url: String,
    registry_address: Address,
}

impl TxManager {
    pub fn new(
        tee_service_signer: Signer,
        funding_signer: Signer,
        rpc_url: String,
        registry_address: Address,
    ) -> Self {
        Self {
            tee_service_signer,
            funding_signer,
            rpc_url,
            registry_address,
        }
    }

    pub async fn fund_address(
        &self,
        from: Signer,
        to: Address,
        amount: U256,
    ) -> Result<(), TxManagerError> {
        let funding_wallet = PrivateKeySigner::from_bytes(&from.secret.secret_bytes().into())
            .map_err(TxManagerError::SignerError)?;
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
                Ok(())
            }
            Err(e) => {
                warn!(target: "flashtestations", error = %e, "funding transaction failed");
                Err(e)
            }
        }
    }

    pub async fn fund_and_register_tee_service(
        &self,
        attestation: Vec<u8>,
        extra_registration_data: Bytes,
        funding_amount: U256,
    ) -> Result<(), TxManagerError> {
        info!(target: "flashtestations", "funding TEE address at {}", self.tee_service_signer.address);
        self.fund_address(
            self.funding_signer,
            self.tee_service_signer.address,
            funding_amount,
        )
        .await
        .unwrap_or_else(|e| {
            warn!(target: "flashtestations", error = %e, "Failed to fund TEE address, attempting to register without funding");
        });

        let quote_bytes = Bytes::from(attestation);
        let wallet =
            PrivateKeySigner::from_bytes(&self.tee_service_signer.secret.secret_bytes().into())
                .map_err(TxManagerError::SignerError)?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .fetch_chain_id()
            .with_gas_estimation()
            .with_cached_nonce_management()
            .wallet(wallet)
            .network::<Optimism>()
            .connect(self.rpc_url.as_str())
            .await?;

        info!(target: "flashtestations", "submitting quote to registry at {}", self.registry_address);

        let calldata = IFlashtestationRegistry::registerTEEServiceCall {
            rawQuote: quote_bytes,
            extendedRegistrationData: extra_registration_data,
        }
        .abi_encode();
        let tx = TransactionRequest {
            from: Some(self.tee_service_signer.address),
            to: Some(TxKind::Call(self.registry_address)),
            input: calldata.into(),
            ..Default::default()
        };
        match Self::process_pending_tx(provider.send_transaction(tx.into()).await).await {
            Ok(tx_hash) => {
                info!(target: "flashtestations", tx_hash = %tx_hash, "attestation transaction confirmed successfully");
                Ok(())
            }
            Err(e) => {
                warn!(target: "flashtestations", error = %e, "attestation transaction failed to be sent");
                Err(e)
            }
        }
    }

    pub async fn clean_up(&self) -> Result<(), TxManagerError> {
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
            .await?;
        }
        Ok(())
    }

    /// Processes a pending transaction and logs whether the transaction succeeded or not
    async fn process_pending_tx(
        pending_tx_result: TransportResult<PendingTransactionBuilder<Optimism>>,
    ) -> Result<TxHash, TxManagerError> {
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
                            Err(TxManagerError::TxReverted(tx_hash))
                        }
                    }
                    Err(e) => Err(TxManagerError::TxConfirmationError(e)),
                }
            }
            Err(e) => Err(TxManagerError::TxRpcError(e)),
        }
    }
}
