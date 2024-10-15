use crate::TxDeposit;
use alloy_primitives::B256;

/// A trait representing a deposit transaction with specific attributes.
pub trait DepositTransaction {
    /// Returns the hash that uniquely identifies the source of the deposit.
    ///
    /// # Returns
    /// An `Option<B256>` containing the source hash if available.
    fn source_hash(&self) -> Option<B256>;

    /// Returns the optional mint value of the deposit transaction.
    ///
    /// # Returns
    /// An `Option<u128>` representing the ETH value to mint on L2, if any.
    fn mint(&self) -> Option<u128>;

    /// Indicates whether the transaction is exempt from the L2 gas limit.
    ///
    /// # Returns
    /// A `bool` indicating if the transaction is a system transaction.
    fn is_system_transaction(&self) -> bool;

    /// Checks if the transaction is a deposit transaction.
    ///
    /// # Returns
    /// A `bool` that is always `true` for deposit transactions.
    fn is_deposit(&self) -> bool;
}

impl DepositTransaction for TxDeposit {
    fn source_hash(&self) -> Option<B256> {
        Some(self.source_hash)
    }

    fn mint(&self) -> Option<u128> {
        self.mint
    }

    fn is_system_transaction(&self) -> bool {
        self.is_system_transaction
    }

    fn is_deposit(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::{traits::DepositTransaction, TxDeposit};
    use alloy_consensus::Transaction;
    use alloy_primitives::{Address, Bytes, TxKind, B256, U256};

    #[test]
    fn test_deposit_transaction_trait() {
        let tx = TxDeposit {
            source_hash: B256::with_last_byte(42),
            from: Address::default(),
            to: TxKind::default(),
            mint: Some(100),
            value: U256::from(1000),
            gas_limit: 50000,
            is_system_transaction: true,
            input: Bytes::default(),
        };

        assert_eq!(tx.source_hash(), Some(B256::with_last_byte(42)));
        assert_eq!(tx.mint(), Some(100));
        assert!(tx.is_system_transaction());
        assert!(tx.is_deposit());
    }

    #[test]
    fn test_deposit_transaction_without_mint() {
        let tx = TxDeposit {
            source_hash: B256::default(),
            from: Address::default(),
            to: TxKind::default(),
            mint: None,
            value: U256::default(),
            gas_limit: 50000,
            is_system_transaction: false,
            input: Bytes::default(),
        };

        assert_eq!(tx.source_hash(), Some(B256::default()));
        assert_eq!(tx.mint(), None);
        assert!(!tx.is_system_transaction());
        assert!(tx.is_deposit());
    }

    #[test]
    fn test_deposit_transaction_to_contract() {
        let contract_address = Address::with_last_byte(0xFF);
        let tx = TxDeposit {
            source_hash: B256::default(),
            from: Address::default(),
            to: TxKind::Call(contract_address),
            mint: Some(200),
            value: U256::from(500),
            gas_limit: 100000,
            is_system_transaction: false,
            input: Bytes::from_static(&[1, 2, 3]),
        };

        assert_eq!(tx.source_hash(), Some(B256::default()));
        assert_eq!(tx.mint(), Some(200));
        assert!(!tx.is_system_transaction());
        assert!(tx.is_deposit());
        assert_eq!(tx.to(), TxKind::Call(contract_address));
    }
}
