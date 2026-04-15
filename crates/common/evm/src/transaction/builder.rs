//! Contains the transaction builder.

use alloc::vec;

use revm::{
    context::{TxEnv, tx::TxEnvBuilder},
    primitives::{B256, Bytes},
};

use super::{
    core::OpTransaction,
    deposit::{DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts},
    error::BuildError,
};

/// Builder for constructing [`OpTransaction`] instances
#[derive(Default, Debug)]
pub struct BaseTransactionBuilder {
    base: TxEnvBuilder,
    enveloped_tx: Option<Bytes>,
    deposit: DepositTransactionParts,
}

impl BaseTransactionBuilder {
    /// Create a new builder with default values
    pub fn new() -> Self {
        Self {
            base: TxEnvBuilder::new(),
            enveloped_tx: None,
            deposit: DepositTransactionParts::default(),
        }
    }

    /// Set the base transaction builder based for `TxEnvBuilder`.
    pub fn base(mut self, base: TxEnvBuilder) -> Self {
        self.base = base;
        self
    }

    /// Set the enveloped transaction bytes.
    pub fn enveloped_tx(mut self, enveloped_tx: Option<Bytes>) -> Self {
        self.enveloped_tx = enveloped_tx;
        self
    }

    /// Set the source hash of the deposit transaction.
    pub const fn source_hash(mut self, source_hash: B256) -> Self {
        self.deposit.source_hash = source_hash;
        self
    }

    /// Set the mint of the deposit transaction.
    pub const fn mint(mut self, mint: u128) -> Self {
        self.deposit.mint = Some(mint);
        self
    }

    /// Set the deposit transaction to be a system transaction.
    pub const fn is_system_transaction(mut self) -> Self {
        self.deposit.is_system_transaction = true;
        self
    }

    /// Set the deposit transaction to not be a system transaction.
    pub const fn not_system_transaction(mut self) -> Self {
        self.deposit.is_system_transaction = false;
        self
    }

    /// Set the deposit transaction to be a deposit transaction.
    pub fn is_deposit_tx(mut self) -> Self {
        self.base = self.base.tx_type(Some(DEPOSIT_TRANSACTION_TYPE));
        self
    }

    /// Build the [`OpTransaction`] with default values for missing fields.
    ///
    /// This is useful for testing and debugging where it is not necessary to
    /// have full [`OpTransaction`] instance.
    ///
    /// If the transaction is a deposit (either `tx_type == DEPOSIT_TRANSACTION_TYPE` or
    /// `source_hash != B256::ZERO`), set the transaction type accordingly and ensure the
    /// `enveloped_tx` is removed (`None`). For non-deposit transactions, ensure
    /// `enveloped_tx` is set.
    pub fn build_fill(mut self) -> OpTransaction<TxEnv> {
        let tx_type = self.base.get_tx_type();
        if tx_type.is_some() {
            if tx_type == Some(DEPOSIT_TRANSACTION_TYPE) {
                // source hash is required for deposit transactions
                if self.deposit.source_hash == B256::ZERO {
                    self.deposit.source_hash = B256::from([1u8; 32]);
                }
                // deposit transactions should not carry enveloped bytes
                self.enveloped_tx = None;
            } else {
                // enveloped is required for non-deposit transactions
                self.enveloped_tx = Some(vec![0x00].into());
            }
        } else if self.deposit.source_hash != B256::ZERO {
            // if type is not set and source hash is set, set the transaction type to deposit
            self.base = self.base.tx_type(Some(DEPOSIT_TRANSACTION_TYPE));
            // deposit transactions should not carry enveloped bytes
            self.enveloped_tx = None;
        } else if self.enveloped_tx.is_none() {
            // if type is not set and source hash is not set, set the enveloped transaction to something.
            self.enveloped_tx = Some(vec![0x00].into());
        }

        let base = self.base.build_fill();

        OpTransaction { base, enveloped_tx: self.enveloped_tx, deposit: self.deposit }
    }

    /// Build the [`OpTransaction`] instance, return error if the transaction is not valid.
    ///
    pub fn build(mut self) -> Result<OpTransaction<TxEnv>, BuildError> {
        let tx_type = self.base.get_tx_type();
        if tx_type.is_some() {
            if Some(DEPOSIT_TRANSACTION_TYPE) == tx_type {
                // if tx type is deposit, check if source hash is set
                if self.deposit.source_hash == B256::ZERO {
                    return Err(BuildError::MissingSourceHashForDeposit);
                }
            } else if self.enveloped_tx.is_none() {
                // enveloped is required for non-deposit transactions
                return Err(BuildError::MissingEnvelopedTxBytes);
            }
        } else if self.deposit.source_hash != B256::ZERO {
            // if type is not set and source hash is set, set the transaction type to deposit
            self.base = self.base.tx_type(Some(DEPOSIT_TRANSACTION_TYPE));
        } else if self.enveloped_tx.is_none() {
            // tx is not deposit and enveloped is required
            return Err(BuildError::MissingEnvelopedTxBytes);
        }

        let base = self.base.build()?;

        Ok(OpTransaction { base, enveloped_tx: self.enveloped_tx, deposit: self.deposit })
    }
}
