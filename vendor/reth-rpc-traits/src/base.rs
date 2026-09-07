//! Conversion of concrete Base requests and responses for shared RPC handlers.

use core::convert::Infallible;

use alloy_consensus::{SignableTransaction, error::ValueError};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Signature, U256};
use alloy_rpc_types_eth::Header;
use base_common_consensus::{BaseTransactionInfo, BaseTxEnvelope};
use base_common_rpc_types::{BaseHeaderResponse, BaseTransactionRequest, Transaction};

use crate::{FromConsensusTx, SignTxRequestError, SignableTxRequest, TryIntoSimTx};

impl FromConsensusTx<BaseTxEnvelope> for Transaction {
    type TxInfo = BaseTransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: BaseTxEnvelope,
        signer: Address,
        tx_info: BaseTransactionInfo,
    ) -> Result<Self, Infallible> {
        Ok(Self::from_transaction(
            alloy_consensus::transaction::Recovered::new_unchecked(tx, signer),
            tx_info,
        ))
    }
}

impl TryIntoSimTx<BaseTxEnvelope> for BaseTransactionRequest {
    fn try_into_sim_tx(self) -> Result<BaseTxEnvelope, ValueError<Self>> {
        let tx = self
            .build_typed_tx()
            .map_err(|request| ValueError::new(request, "Required fields missing"))?;

        // Create an empty signature for the transaction.
        let signature = Signature::new(Default::default(), Default::default(), false);

        Ok(tx.into_signed(signature).into())
    }
}

impl SignableTxRequest<BaseTxEnvelope> for BaseTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<BaseTxEnvelope, SignTxRequestError> {
        let mut tx =
            self.build_typed_tx().map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;

        // sanity check: deposit transactions must not be signed by the user
        if tx.is_deposit() {
            return Err(SignTxRequestError::InvalidTransactionRequest);
        }

        let signature = signer.sign_transaction(&mut tx).await?;

        Ok(tx.into_signed(signature).into())
    }
}

impl crate::FromConsensusHeader for BaseHeaderResponse<Header> {
    fn from_consensus_header(
        header: reth_primitives_traits::SealedHeader,
        block_size: usize,
    ) -> Self {
        Self::new(Header::from_consensus(header.into(), None, Some(U256::from(block_size))))
    }
}
