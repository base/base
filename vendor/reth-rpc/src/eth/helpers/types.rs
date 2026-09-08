//! L1 `eth` API types.

//tests for simulate
#[cfg(test)]
mod tests {
    use alloy_consensus::Transaction;
    use alloy_rpc_types_eth::TransactionRequest;
    use reth_provider::test_utils::MockEthProvider;
    use reth_rpc_eth_api::BaseRpcConverter;
    use reth_rpc_eth_types::simulate::resolve_transaction;
    use revm::database::CacheDB;

    #[test]
    fn test_resolve_transaction_empty_request() {
        let builder = BaseRpcConverter::new(MockEthProvider::default(), Default::default());
        let mut db = CacheDB::<revm::database::EmptyDBTyped<reth_errors::ProviderError>>::default();
        let tx = TransactionRequest::default();
        let result = resolve_transaction(tx.into(), 21000, 0, 1, false, &mut db, &builder).unwrap();

        // For an empty request, we should get a valid transaction with defaults
        let tx = result.into_inner();
        assert_eq!(tx.max_fee_per_gas(), 0);
        assert_eq!(tx.max_priority_fee_per_gas(), Some(0));
        assert_eq!(tx.gas_price(), None);
    }

    #[test]
    fn test_resolve_transaction_legacy() {
        let mut db = CacheDB::<revm::database::EmptyDBTyped<reth_errors::ProviderError>>::default();
        let builder = BaseRpcConverter::new(MockEthProvider::default(), Default::default());

        let tx = TransactionRequest { gas_price: Some(100), ..Default::default() };

        let tx = resolve_transaction(tx.into(), 21000, 0, 1, false, &mut db, &builder).unwrap();

        assert_eq!(tx.tx_type(), base_common_consensus::OpTxType::Legacy);

        let tx = tx.into_inner();
        assert_eq!(tx.gas_price(), Some(100));
        assert_eq!(tx.max_priority_fee_per_gas(), None);
    }

    #[test]
    fn test_resolve_transaction_partial_eip1559() {
        let mut db = CacheDB::<revm::database::EmptyDBTyped<reth_errors::ProviderError>>::default();
        let rpc_converter = BaseRpcConverter::new(MockEthProvider::default(), Default::default());

        let tx = TransactionRequest {
            max_fee_per_gas: Some(200),
            max_priority_fee_per_gas: Some(10),
            ..Default::default()
        };

        let result =
            resolve_transaction(tx.into(), 21000, 0, 1, false, &mut db, &rpc_converter).unwrap();

        assert_eq!(result.tx_type(), base_common_consensus::OpTxType::Eip1559);
        let tx = result.into_inner();
        assert_eq!(tx.max_fee_per_gas(), 200);
        assert_eq!(tx.max_priority_fee_per_gas(), Some(10));
        assert_eq!(tx.gas_price(), None);
    }

    #[test]
    fn test_resolve_transaction_wraps_max_nonce_when_nonce_check_disabled() {
        let mut db = CacheDB::<revm::database::EmptyDBTyped<reth_errors::ProviderError>>::default();
        let rpc_converter = BaseRpcConverter::new(MockEthProvider::default(), Default::default());

        let tx = TransactionRequest { nonce: Some(u64::MAX), ..Default::default() };

        let result =
            resolve_transaction(tx.into(), 21000, 0, 1, true, &mut db, &rpc_converter).unwrap();

        assert_eq!(result.nonce(), 0);
    }
}
