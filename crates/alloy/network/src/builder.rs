use alloy_consensus::TxType;
use alloy_network::{BuildResult, TransactionBuilder, TransactionBuilderError};
use alloy_primitives::{Address, Bytes, ChainId, TxKind, U256};
use alloy_rpc_types_eth::AccessList;
use base_alloy_consensus::{OpTxType, OpTypedTransaction};
use base_alloy_rpc_types::OpTransactionRequest;

use crate::Base;

impl TransactionBuilder<Base> for OpTransactionRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.as_ref().chain_id()
    }

    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.as_mut().set_chain_id(chain_id);
    }

    fn nonce(&self) -> Option<u64> {
        self.as_ref().nonce()
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.as_mut().set_nonce(nonce);
    }

    fn take_nonce(&mut self) -> Option<u64> {
        self.as_mut().nonce.take()
    }

    fn input(&self) -> Option<&Bytes> {
        self.as_ref().input()
    }

    fn set_input<T: Into<Bytes>>(&mut self, input: T) {
        self.as_mut().set_input(input);
    }

    fn from(&self) -> Option<Address> {
        self.as_ref().from()
    }

    fn set_from(&mut self, from: Address) {
        self.as_mut().set_from(from);
    }

    fn kind(&self) -> Option<TxKind> {
        self.as_ref().kind()
    }

    fn clear_kind(&mut self) {
        self.as_mut().clear_kind();
    }

    fn set_kind(&mut self, kind: TxKind) {
        self.as_mut().set_kind(kind);
    }

    fn value(&self) -> Option<U256> {
        self.as_ref().value()
    }

    fn set_value(&mut self, value: U256) {
        self.as_mut().set_value(value);
    }

    fn gas_price(&self) -> Option<u128> {
        self.as_ref().gas_price()
    }

    fn set_gas_price(&mut self, gas_price: u128) {
        self.as_mut().set_gas_price(gas_price);
    }

    fn max_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_fee_per_gas()
    }

    fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) {
        self.as_mut().set_max_fee_per_gas(max_fee_per_gas);
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_priority_fee_per_gas()
    }

    fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) {
        self.as_mut().set_max_priority_fee_per_gas(max_priority_fee_per_gas);
    }

    fn gas_limit(&self) -> Option<u64> {
        self.as_ref().gas_limit()
    }

    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.as_mut().set_gas_limit(gas_limit);
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.as_ref().access_list()
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.as_mut().set_access_list(access_list);
    }

    fn complete_type(&self, ty: OpTxType) -> Result<(), Vec<&'static str>> {
        match ty {
            OpTxType::Deposit => Err(vec!["not implemented for deposit tx"]),
            _ => {
                let ty = TxType::try_from(ty as u8).map_err(|_| vec!["unsupported tx type"])?;
                self.as_ref().complete_type(ty)
            }
        }
    }

    fn can_submit(&self) -> bool {
        self.as_ref().can_submit()
    }

    fn can_build(&self) -> bool {
        self.as_ref().can_build()
    }

    #[doc(alias = "output_transaction_type")]
    fn output_tx_type(&self) -> OpTxType {
        match self.as_ref().preferred_type() {
            TxType::Eip1559 | TxType::Eip4844 => OpTxType::Eip1559,
            TxType::Eip2930 => OpTxType::Eip2930,
            TxType::Eip7702 => OpTxType::Eip7702,
            TxType::Legacy => OpTxType::Legacy,
        }
    }

    #[doc(alias = "output_transaction_type_checked")]
    fn output_tx_type_checked(&self) -> Option<OpTxType> {
        self.as_ref().buildable_type().map(|tx_ty| match tx_ty {
            TxType::Eip1559 | TxType::Eip4844 => OpTxType::Eip1559,
            TxType::Eip2930 => OpTxType::Eip2930,
            TxType::Eip7702 => OpTxType::Eip7702,
            TxType::Legacy => OpTxType::Legacy,
        })
    }

    fn prep_for_submission(&mut self) {
        self.as_mut().prep_for_submission();
    }

    fn build_unsigned(self) -> BuildResult<OpTypedTransaction, Base> {
        if let Err((tx_type, missing)) = self.as_ref().missing_keys() {
            let tx_type = OpTxType::try_from(tx_type as u8).map_err(|e| {
                TransactionBuilderError::<Base>::custom(e).into_unbuilt(self.clone())
            })?;
            return Err(TransactionBuilderError::InvalidTransactionRequest(tx_type, missing)
                .into_unbuilt(self));
        }
        Ok(self.build_typed_tx().expect("checked by missing_keys"))
    }

    async fn build<W: alloy_network::NetworkWallet<Base>>(
        self,
        wallet: &W,
    ) -> Result<<Base as alloy_network::Network>::TxEnvelope, TransactionBuilderError<Base>> {
        Ok(wallet.sign_request(self).await?)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use rstest::rstest;

    use super::{OpTxType, OpTypedTransaction};

    /// Returns a minimal valid EIP-1559 [`OpTransactionRequest`].
    fn complete_eip1559_request() -> OpTransactionRequest {
        let mut req = OpTransactionRequest::default();
        req.set_nonce(0);
        req.set_gas_limit(21_000);
        req.set_max_fee_per_gas(1);
        req.set_max_priority_fee_per_gas(1);
        req.set_chain_id(1);
        req.set_kind(TxKind::Create);
        req
    }

    #[rstest]
    #[case::legacy(OpTxType::Legacy)]
    #[case::eip2930(OpTxType::Eip2930)]
    #[case::eip1559(OpTxType::Eip1559)]
    #[case::eip7702(OpTxType::Eip7702)]
    fn complete_type_delegates_for_eth_types(#[case] ty: OpTxType) {
        let req = OpTransactionRequest::default();
        // Should not panic — returns Ok or missing-fields Err from the inner request.
        let _ = req.complete_type(ty);
    }

    #[test]
    fn complete_type_rejects_deposit() {
        let req = OpTransactionRequest::default();
        let err = req.complete_type(OpTxType::Deposit).unwrap_err();
        assert_eq!(err, vec!["not implemented for deposit tx"]);
    }

    #[test]
    fn build_unsigned_succeeds_with_complete_request() {
        let req = complete_eip1559_request();
        let tx = req.build_unsigned().unwrap();
        assert!(matches!(tx, OpTypedTransaction::Eip1559(_)));
    }

    #[test]
    fn build_unsigned_returns_missing_keys_error() {
        let req = OpTransactionRequest::default();
        let err = req.build_unsigned().unwrap_err();
        assert!(matches!(
            err.error,
            TransactionBuilderError::InvalidTransactionRequest(OpTxType::Eip1559, _)
        ));
    }

    #[test]
    fn build_unsigned_returns_custom_error_for_unmappable_tx_type() {
        // Force preferred_type() to return TxType::Eip4844 (u8 = 3), which has
        // no corresponding OpTxType variant.
        let mut req = OpTransactionRequest::default();
        req.as_mut().blob_versioned_hashes = Some(vec![B256::ZERO]);
        let err = req.build_unsigned().unwrap_err();
        assert!(matches!(err.error, TransactionBuilderError::Custom(_)));
    }
}
