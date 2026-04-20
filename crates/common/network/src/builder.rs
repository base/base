use alloy_consensus::TxType;
use alloy_network::{BuildResult, NetworkTransactionBuilder, TransactionBuilder, TransactionBuilderError};
use base_common_consensus::{BaseTypedTransaction, OpTxType};
use base_common_rpc_types::BaseTransactionRequest;

use crate::Base;

impl NetworkTransactionBuilder<Base> for BaseTransactionRequest {
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

    fn build_unsigned(self) -> BuildResult<BaseTypedTransaction, Base> {
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
    use alloy_primitives::TxKind;
    use rstest::rstest;

    use super::*;

    /// Returns a minimal valid EIP-1559 [`BaseTransactionRequest`].
    fn complete_eip1559_request() -> BaseTransactionRequest {
        let mut req = BaseTransactionRequest::default();
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
        let req = BaseTransactionRequest::default();
        // Should not panic — returns Ok or missing-fields Err from the inner request.
        let _ = req.complete_type(ty);
    }

    #[test]
    fn complete_type_rejects_deposit() {
        let req = BaseTransactionRequest::default();
        let err = req.complete_type(OpTxType::Deposit).unwrap_err();
        assert_eq!(err, vec!["not implemented for deposit tx"]);
    }

    #[test]
    fn build_unsigned_succeeds_with_complete_request() {
        let req = complete_eip1559_request();
        let tx = req.build_unsigned().unwrap();
        assert!(matches!(tx, BaseTypedTransaction::Eip1559(_)));
    }

    #[test]
    fn build_unsigned_returns_missing_keys_error() {
        let req = BaseTransactionRequest::default();
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
        let mut req = BaseTransactionRequest::default();
        req.as_mut().blob_versioned_hashes = Some(vec![B256::ZERO]);
        let err = req.build_unsigned().unwrap_err();
        assert!(matches!(err.error, TransactionBuilderError::Custom(_)));
    }
}
