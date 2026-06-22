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

use crate::{
    BaseTransactionBuilder, BaseTxTr, DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts,
    EIP8130_TRANSACTION_TYPE, Eip8130TransactionParts,
};

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
    /// EIP-8130 account-abstraction transaction parts.
    ///
    /// `Some` only for EIP-8130 transactions, carrying the full signed envelope
    /// the handler needs to run the authorize → apply → execute pipeline (the
    /// `base` `TxEnv` is only a placeholder projection for such transactions).
    pub eip8130: Option<Eip8130TransactionParts>,
}

impl<T: Transaction> AsRef<T> for BaseTransaction<T> {
    fn as_ref(&self) -> &T {
        &self.base
    }
}

impl<T: Transaction> BaseTransaction<T> {
    /// Create a new Base transaction.
    pub fn new(base: T) -> Self {
        Self {
            base,
            enveloped_tx: None,
            deposit: DepositTransactionParts::default(),
            eip8130: None,
        }
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
            eip8130: None,
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
        } else if self.eip8130.is_some() {
            EIP8130_TRANSACTION_TYPE
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
        // EIP-8130 transactions have no top-level value; the base env is a
        // placeholder and any per-call value lives in the signed envelope.
        debug_assert!(
            self.eip8130.is_none(),
            "value() called on an EIP-8130 transaction; use the signed envelope"
        );
        self.base.value()
    }

    fn input(&self) -> &Bytes {
        // EIP-8130 transactions have no top-level input; see `value`.
        debug_assert!(
            self.eip8130.is_none(),
            "input() called on an EIP-8130 transaction; use the signed envelope"
        );
        self.base.input()
    }

    fn nonce(&self) -> u64 {
        // EIP-8130 transactions use a 2D nonce; the base env carries only
        // `nonce_sequence` and drops `nonce_key`, so this is a misleading
        // placeholder (see `value`). Use the signed envelope.
        debug_assert!(
            self.eip8130.is_none(),
            "nonce() called on an EIP-8130 transaction; use the signed envelope"
        );
        self.base.nonce()
    }

    fn kind(&self) -> TxKind {
        // EIP-8130 transactions have no single call target; see `value`.
        debug_assert!(
            self.eip8130.is_none(),
            "kind() called on an EIP-8130 transaction; use the signed envelope"
        );
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

    fn eip8130_parts(&self) -> Option<&Eip8130TransactionParts> {
        self.eip8130.as_ref()
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
impl<T: reth_evm::TransactionEnvMut> reth_evm::TransactionEnvMut for BaseTransaction<T> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.set_gas_limit(gas_limit);
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
                eip8130: None,
            },
            BaseTxEnvelope::Eip1559(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: None,
            },
            BaseTxEnvelope::Eip2930(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: None,
            },
            BaseTxEnvelope::Eip7702(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: None,
            },
            BaseTxEnvelope::Eip8130(signed) => {
                // An EIP-8130 transaction has no single call to project into a
                // `TxEnv`. The `base` env is a placeholder carrying only the
                // fields shared by every transaction (caller, gas, fee caps,
                // chain id); execution is driven from the signed envelope in
                // `eip8130` by [`Eip8130Executor`], not from this `TxEnv`.
                //
                // Consequently the `Transaction` accessors backed by the base
                // env that have no EIP-8130 analogue — `kind`, `value`, and
                // `input` — return `TxEnv` defaults (a zero-address call, zero
                // value, empty input) for an 8130 transaction and must not be
                // relied on for execution; `effective_gas_price` stays correct
                // because the fee caps are projected above. The handler routes
                // 8130 transactions to the executor before any of these are hit
                // (see `BaseEvm::transact_raw`).
                //
                // `signed.clone()` is required because `from_encoded_tx` borrows
                // the envelope by shared reference (signature inherited from
                // alloy-evm). The clone deep-copies the account-change and call
                // vectors; the auth blobs are ref-counted `Bytes`.
                let inner = signed.tx();
                let base = TxEnv {
                    caller,
                    gas_limit: inner.gas_limit,
                    gas_price: inner.max_fee_per_gas,
                    gas_priority_fee: Some(inner.max_priority_fee_per_gas),
                    chain_id: Some(inner.chain_id),
                    nonce: inner.nonce_sequence,
                    ..Default::default()
                };
                Self {
                    base,
                    enveloped_tx: Some(encoded),
                    deposit: Default::default(),
                    eip8130: Some(Eip8130TransactionParts::new(signed.clone())),
                }
            }
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
        Self { base, enveloped_tx: Some(encoded), deposit, eip8130: None }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::Encodable2718;
    use base_common_consensus::{BaseTxEnvelope, Eip8130Signed, TxEip8130};
    use revm::{
        context_interface::Transaction,
        primitives::{Address, B256, Bytes},
    };

    use super::*;

    #[test]
    fn test_deposit_transaction_fields() {
        let base_tx = TxEnv::builder().gas_limit(10).gas_price(100).gas_priority_fee(Some(5));

        let base_tx = BaseTransaction::builder()
            .base(base_tx)
            .enveloped_tx(None)
            .not_system_transaction()
            .mint(0u128)
            .source_hash(B256::from([1u8; 32]))
            .build()
            .unwrap();
        // Verify transaction type (deposit transactions should have tx_type based on BaseSpecId)
        // The tx_type is derived from the transaction structure, not set manually
        // Verify common fields access
        assert_eq!(base_tx.gas_limit(), 10);
        assert_eq!(base_tx.kind(), revm::primitives::TxKind::Call(Address::ZERO));
        // Verify gas related calculations - deposit transactions use gas_price for effective gas price
        assert_eq!(base_tx.effective_gas_price(90), 100);
        assert_eq!(base_tx.max_fee_per_gas(), 100);
    }

    /// An EIP-8130 envelope projects to a placeholder `TxEnv` carrying the shared
    /// fields, reports the EIP-8130 transaction type, and retains the full signed
    /// envelope in `eip8130` for the handler.
    #[test]
    fn from_encoded_eip8130_populates_parts() {
        let inner = TxEip8130 {
            chain_id: 8453,
            gas_limit: 250_000,
            max_fee_per_gas: 5_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            nonce_sequence: 7,
            ..Default::default()
        };
        let signed = Eip8130Signed::new(inner, Bytes::from(vec![0u8; 65]), Bytes::new());
        let envelope = BaseTxEnvelope::Eip8130(signed.clone());
        let caller = Address::with_last_byte(0xab);
        let encoded: Bytes = envelope.encoded_2718().into();

        let tx = BaseTransaction::from_encoded_tx(&envelope, caller, encoded);

        assert_eq!(tx.tx_type(), EIP8130_TRANSACTION_TYPE);
        assert!(tx.is_eip8130());
        assert_eq!(tx.eip8130_parts().map(|p| &p.signed), Some(&signed));
        assert_eq!(tx.caller(), caller);
        assert_eq!(tx.gas_limit(), 250_000);
        assert_eq!(tx.chain_id(), Some(8453));
        assert_eq!(tx.max_fee_per_gas(), 5_000_000_000);
        // `nonce()` is a guarded placeholder for 8130 (it drops `nonce_key`);
        // assert the projection on the base env directly to avoid the guard.
        assert_eq!(tx.base.nonce(), 7);
    }
}
