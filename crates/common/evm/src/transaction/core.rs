//! Base transaction abstraction containing the `[BaseTxTr]` trait and corresponding `[BaseTransaction]` type.

use alloc::vec;

use alloy_eips::Encodable2718;
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded, tx::IntoTxEnv};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    handler::SystemCallTx,
    primitives::{Address, B256, Bytes, TxKind, U256},
};

use crate::{BaseTransactionBuilder, BaseTxTr, DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts};

/// Base transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BaseTransaction<T: Transaction> {
    /// Base transaction fields.
    pub base: T,
    /// An enveloped EIP-2718 typed transaction
    ///
    /// This is used to compute the L1 tx cost using the L1 block info, as
    /// opposed to requiring downstream apps to compute the cost
    /// externally.
    pub enveloped_tx: Option<Bytes>,
    /// Deposit transaction parts.
    pub deposit: DepositTransactionParts,
}

impl<T: Transaction> AsRef<T> for BaseTransaction<T> {
    fn as_ref(&self) -> &T {
        &self.base
    }
}

impl<T: Transaction> BaseTransaction<T> {
    /// Create a new Base transaction.
    pub fn new(base: T) -> Self {
        Self { base, enveloped_tx: None, deposit: DepositTransactionParts::default() }
    }
}

impl BaseTransaction<TxEnv> {
    /// Create a new Base transaction.
    pub fn builder() -> BaseTransactionBuilder {
        BaseTransactionBuilder::new()
    }
}

impl Default for BaseTransaction<TxEnv> {
    fn default() -> Self {
        Self {
            base: TxEnv::default(),
            enveloped_tx: Some(vec![0x00].into()),
            deposit: DepositTransactionParts::default(),
        }
    }
}

impl<TX: Transaction + SystemCallTx> SystemCallTx for BaseTransaction<TX> {
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self {
        let mut tx =
            Self::new(TX::new_system_tx_with_caller(caller, system_contract_address, data));

        tx.enveloped_tx = Some(Bytes::default());

        tx
    }
}

impl<T: Transaction> Transaction for BaseTransaction<T> {
    type AccessListItem<'a>
        = T::AccessListItem<'a>
    where
        T: 'a;
    type Authorization<'a>
        = T::Authorization<'a>
    where
        T: 'a;

    fn tx_type(&self) -> u8 {
        // If this is a deposit transaction (has source_hash set), return deposit type
        if self.deposit.source_hash != B256::ZERO {
            DEPOSIT_TRANSACTION_TYPE
        } else {
            self.base.tx_type()
        }
    }

    fn caller(&self) -> Address {
        self.base.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.base.gas_limit()
    }

    fn value(&self) -> U256 {
        self.base.value()
    }

    fn input(&self) -> &Bytes {
        self.base.input()
    }

    fn nonce(&self) -> u64 {
        self.base.nonce()
    }

    fn kind(&self) -> TxKind {
        self.base.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.base.access_list()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.base.max_priority_fee_per_gas()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.base.max_fee_per_gas()
    }

    fn gas_price(&self) -> u128 {
        self.base.gas_price()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.base.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        // Deposit transactions use gas_price directly
        if self.tx_type() == DEPOSIT_TRANSACTION_TYPE {
            return self.gas_price();
        }
        self.base.effective_gas_price(base_fee)
    }

    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.base.authorization_list()
    }
}

impl<T: Transaction> BaseTxTr for BaseTransaction<T> {
    fn enveloped_tx(&self) -> Option<&Bytes> {
        self.enveloped_tx.as_ref()
    }

    fn source_hash(&self) -> Option<B256> {
        if self.tx_type() != DEPOSIT_TRANSACTION_TYPE {
            return None;
        }
        Some(self.deposit.source_hash)
    }

    fn mint(&self) -> Option<u128> {
        self.deposit.mint
    }

    fn is_system_transaction(&self) -> bool {
        self.deposit.is_system_transaction
    }
}

impl<T> IntoTxEnv<Self> for BaseTransaction<T>
where
    T: Transaction,
{
    fn into_tx_env(self) -> Self {
        self
    }
}

#[cfg(feature = "reth")]
impl<T: reth_evm::TransactionEnv> reth_evm::TransactionEnv for BaseTransaction<T> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.set_gas_limit(gas_limit);
    }

    fn nonce(&self) -> u64 {
        reth_evm::TransactionEnv::nonce(&self.base)
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.base.set_nonce(nonce);
    }

    fn set_access_list(&mut self, access_list: revm::context_interface::transaction::AccessList) {
        self.base.set_access_list(access_list);
    }
}

impl FromRecoveredTx<BaseTxEnvelope> for BaseTransaction<TxEnv> {
    fn from_recovered_tx(tx: &BaseTxEnvelope, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<BaseTxEnvelope> for BaseTransaction<TxEnv> {
    fn from_encoded_tx(tx: &BaseTxEnvelope, caller: Address, encoded: Bytes) -> Self {
        match tx {
            BaseTxEnvelope::Legacy(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Eip1559(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Eip2930(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Eip7702(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
            },
            BaseTxEnvelope::Deposit(tx) => Self::from_encoded_tx(tx.inner(), caller, encoded),
        }
    }
}

impl FromRecoveredTx<TxDeposit> for BaseTransaction<TxEnv> {
    fn from_recovered_tx(tx: &TxDeposit, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<TxDeposit> for BaseTransaction<TxEnv> {
    fn from_encoded_tx(tx: &TxDeposit, caller: Address, encoded: Bytes) -> Self {
        let base = TxEnv::from_recovered_tx(tx, caller);
        let deposit = DepositTransactionParts {
            source_hash: tx.source_hash,
            mint: Some(tx.mint),
            is_system_transaction: tx.is_system_transaction,
        };
        Self { base, enveloped_tx: Some(encoded), deposit }
    }
}

#[cfg(test)]
mod tests {
    use revm::{
        context_interface::Transaction,
        primitives::{Address, B256},
    };

    use super::*;

    #[test]
    fn test_deposit_transaction_fields() {
        let base_tx = TxEnv::builder().gas_limit(10).gas_price(100).gas_priority_fee(Some(5));

        let op_tx = BaseTransaction::builder()
            .base(base_tx)
            .enveloped_tx(None)
            .not_system_transaction()
            .mint(0u128)
            .source_hash(B256::from([1u8; 32]))
            .build()
            .unwrap();
        // Verify transaction type (deposit transactions should have tx_type based on OpSpecId)
        // The tx_type is derived from the transaction structure, not set manually
        // Verify common fields access
        assert_eq!(op_tx.gas_limit(), 10);
        assert_eq!(op_tx.kind(), revm::primitives::TxKind::Call(Address::ZERO));
        // Verify gas related calculations - deposit transactions use gas_price for effective gas price
        assert_eq!(op_tx.effective_gas_price(90), 100);
        assert_eq!(op_tx.max_fee_per_gas(), 100);
    }
}
