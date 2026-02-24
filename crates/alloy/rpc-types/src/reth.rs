//! Reth compatibility implementations for RPC types.

use core::convert::Infallible;

use alloy_consensus::{SignableTransaction, error::ValueError};
use alloy_evm::{
    EvmEnv,
    env::BlockEnvironment,
    rpc::{EthTxEnvError, TryIntoTxEnv},
};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes};
use alloy_signer::Signature;
use base_alloy_consensus::{OpTransaction, OpTransactionInfo, OpTxEnvelope};
use base_revm::OpTransaction as OpRevm;
use reth_rpc_convert::{
    SignTxRequestError, SignableTxRequest, TryIntoSimTx, transaction::FromConsensusTx,
};
use revm::context::TxEnv;

use crate::{OpTransactionRequest, Transaction};

impl<T: OpTransaction + alloy_consensus::Transaction> FromConsensusTx<T> for Transaction<T> {
    type TxInfo = OpTransactionInfo;
    type Err = Infallible;

    fn from_consensus_tx(
        tx: T,
        signer: Address,
        tx_info: OpTransactionInfo,
    ) -> Result<Self, Infallible> {
        Ok(Self::from_transaction(
            alloy_consensus::transaction::Recovered::new_unchecked(tx, signer),
            tx_info,
        ))
    }
}

impl<Block: BlockEnvironment> TryIntoTxEnv<OpRevm<TxEnv>, Block> for OpTransactionRequest {
    type Err = EthTxEnvError;

    fn try_into_tx_env<Spec>(
        self,
        evm_env: &EvmEnv<Spec, Block>,
    ) -> Result<OpRevm<TxEnv>, Self::Err> {
        Ok(OpRevm {
            base: self.as_ref().clone().try_into_tx_env(evm_env)?,
            enveloped_tx: Some(Bytes::new()),
            deposit: Default::default(),
        })
    }
}

impl TryIntoSimTx<OpTxEnvelope> for OpTransactionRequest {
    fn try_into_sim_tx(self) -> Result<OpTxEnvelope, ValueError<Self>> {
        let tx = self
            .build_typed_tx()
            .map_err(|request| ValueError::new(request, "Required fields missing"))?;

        // Create an empty signature for the transaction.
        let signature = Signature::new(Default::default(), Default::default(), false);

        Ok(tx.into_signed(signature).into())
    }
}

impl SignableTxRequest<OpTxEnvelope> for OpTransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl TxSigner<Signature> + Send,
    ) -> Result<OpTxEnvelope, SignTxRequestError> {
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
