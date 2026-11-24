//! Contains the transaction builder.

use alloc::vec;

use revm::{
    context::{TxEnv, tx::TxEnvBuilder},
    primitives::{B256, Bytes},
};

use super::{
    core::BaseTransaction,
    deposit::{DEPOSIT_TRANSACTION_TYPE, DepositTransactionParts},
    error::BuildError,
};

/// Builder for constructing [`BaseTransaction`] instances
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

    /// Build the [`BaseTransaction`] with default values for missing fields.
    ///
    /// This is useful for testing and debugging where it is not necessary to
    /// have full [`BaseTransaction`] instance.
    ///
    /// If the transaction is a deposit (either `tx_type == DEPOSIT_TRANSACTION_TYPE` or
    /// `source_hash != B256::ZERO`), set the transaction type accordingly and ensure the
    /// `enveloped_tx` is removed (`None`). For non-deposit transactions, ensure
    /// `enveloped_tx` is set.
    pub fn build_fill(mut self) -> BaseTransaction<TxEnv> {
        let tx_type = self.base.get_tx_type();
        if tx_type.is_some() {
            if tx_type == Some(DEPOSIT_TRANSACTION_TYPE) {
                if self.deposit.source_hash == B256::ZERO {
                    self.deposit.source_hash = B256::from([1u8; 32]);
                }
                self.enveloped_tx = None;
            } else {
                self.enveloped_tx = Some(vec![0x00].into());
            }
        } else if self.deposit.source_hash != B256::ZERO {
            self.base = self.base.tx_type(Some(DEPOSIT_TRANSACTION_TYPE));
            self.enveloped_tx = None;
        } else if self.enveloped_tx.is_none() {
            self.enveloped_tx = Some(vec![0x00].into());
        }

        let base = self.base.build_fill();

        BaseTransaction {
            base,
            enveloped_tx: self.enveloped_tx,
            enveloped_tx_fastlz_size: None,
            deposit: self.deposit,
            base_tx: None,
        }
    }

    /// Build the [`BaseTransaction`] instance, return error if the transaction is not valid.
    ///
    pub fn build(mut self) -> Result<BaseTransaction<TxEnv>, BuildError> {
        let tx_type = self.base.get_tx_type();
        if tx_type.is_some() {
            if Some(DEPOSIT_TRANSACTION_TYPE) == tx_type {
                if self.deposit.source_hash == B256::ZERO {
                    return Err(BuildError::MissingSourceHashForDeposit);
                }
            } else if self.enveloped_tx.is_none() {
                return Err(BuildError::MissingEnvelopedTxBytes);
            }
        } else if self.deposit.source_hash != B256::ZERO {
            self.base = self.base.tx_type(Some(DEPOSIT_TRANSACTION_TYPE));
        } else if self.enveloped_tx.is_none() {
            return Err(BuildError::MissingEnvelopedTxBytes);
        }

        let base = self.base.build()?;

        Ok(BaseTransaction {
            base,
            enveloped_tx: self.enveloped_tx,
            enveloped_tx_fastlz_size: None,
            deposit: self.deposit,
            base_tx: None,
        })
    }
}
