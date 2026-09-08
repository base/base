use std::ops::{Deref, DerefMut};

use alloy_rpc_types_eth::{TransactionRequest};
use alloy_serde::WithOtherFields;

use crate::{
    BuildResult, Network, NetworkTransactionBuilder, NetworkWallet, TransactionBuilder,
    TransactionBuilderError, any::AnyNetwork,
};

impl TransactionBuilder for WithOtherFields<TransactionRequest> {
    fn transaction_request(&self) -> &TransactionRequest {
        &self.inner
    }
    fn transaction_request_mut(&mut self) -> &mut TransactionRequest {
        &mut self.inner
    }
}

impl NetworkTransactionBuilder<AnyNetwork> for WithOtherFields<TransactionRequest> {
    fn can_submit(&self) -> bool {
        self.deref().can_submit()
    }

    fn can_build(&self) -> bool {
        self.deref().can_build()
    }

    fn complete_type(&self, ty: <AnyNetwork as Network>::TxType) -> Result<(), Vec<&'static str>> {
        self.deref().complete_type(ty.try_into().map_err(|_| vec!["unsupported_transaction_type"])?)
    }

    #[doc(alias = "output_transaction_type")]
    fn output_tx_type(&self) -> <AnyNetwork as Network>::TxType {
        self.deref().output_tx_type().into()
    }

    #[doc(alias = "output_transaction_type_checked")]
    fn output_tx_type_checked(&self) -> Option<<AnyNetwork as Network>::TxType> {
        self.deref().output_tx_type_checked().map(Into::into)
    }

    fn prep_for_submission(&mut self) {
        self.deref_mut().prep_for_submission()
    }

    /// Build an unsigned typed transaction.
    ///
    /// This method validates that all required fields are present and builds an
    /// unsigned transaction. Returns an error if any required fields are missing.
    ///
    /// # Limitations
    ///
    /// The [`TransactionRequest`] can only build Ethereum transaction types
    /// (Legacy, EIP-2930, EIP-1559, EIP-4844, EIP-7702). Attempting to build
    /// unknown transaction types will result in an error.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionBuilderError::InvalidTransactionRequest`] if required
    /// fields are missing for the transaction type.
    fn build_unsigned(self) -> BuildResult<<AnyNetwork as Network>::UnsignedTx, AnyNetwork> {
        if let Err((tx_type, missing)) = self.missing_keys() {
            return Err(TransactionBuilderError::InvalidTransactionRequest(
                tx_type.into(),
                missing,
            )
            .into_unbuilt(self));
        }
        Ok(self.inner.build_typed_tx().expect("checked by missing_keys").into())
    }

    /// Build and sign a transaction using the provided wallet.
    ///
    /// This method signs the transaction request with the given wallet and returns
    /// a signed transaction envelope ready for submission to the network.
    ///
    /// # Limitations
    ///
    /// The [`TransactionRequest`] can only build Ethereum transaction types.
    /// Unknown transaction types cannot be signed through this builder.
    ///
    /// # Errors
    ///
    /// Returns an error if signing fails or if the wallet cannot produce the
    /// required signature type for the transaction.
    async fn build<W: NetworkWallet<AnyNetwork>>(
        self,
        wallet: &W,
    ) -> Result<<AnyNetwork as Network>::TxEnvelope, TransactionBuilderError<AnyNetwork>> {
        Ok(wallet.sign_request(self).await?)
    }
}
