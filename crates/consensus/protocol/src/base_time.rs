//! `BaseTime` metadata deposit transaction encoding.

use alloc::vec::Vec;

use alloy_consensus::TxReceipt;
use alloy_primitives::{Bytes, Sealable, Sealed, TxKind, U256};
use base_common_consensus::{
    BaseTimeDepositSource, BaseTransaction, DepositSourceDomain, Predeploys, SystemAddresses,
    TxDeposit,
};

use crate::REGOLITH_SYSTEM_TX_GAS;

/// Versioned calldata for the `BaseTime` metadata deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BaseTimeUpdateTx {
    /// The sub-second millisecond component for the block timestamp.
    timestamp_millis_part: u16,
}

impl BaseTimeUpdateTx {
    /// Milliseconds between consecutive `BaseTime` slots.
    pub const BLOCK_INTERVAL_MILLIS: u16 = 200;

    /// The selector for `setTimestampMillisPart(uint16)`.
    pub const SELECTOR: [u8; 4] = [0x86, 0xbd, 0xf3, 0x94];

    /// The ABI calldata length.
    pub const CALLDATA_LEN: usize = 4 + 32;

    /// Returns whether a millisecond component is aligned to a `BaseTime` slot.
    pub const fn is_valid_timestamp_millis_part(timestamp_millis_part: u16) -> bool {
        timestamp_millis_part < 1_000
            && timestamp_millis_part.is_multiple_of(Self::BLOCK_INTERVAL_MILLIS)
    }

    /// Creates a new [`BaseTimeUpdateTx`].
    pub const fn new(timestamp_millis_part: u16) -> Result<Self, BaseTimeUpdateError> {
        if !Self::is_valid_timestamp_millis_part(timestamp_millis_part) {
            return Err(BaseTimeUpdateError::InvalidTimestampMillisPart(timestamp_millis_part));
        }

        Ok(Self { timestamp_millis_part })
    }

    /// Returns the validated sub-second millisecond component for the block timestamp.
    pub const fn timestamp_millis_part(&self) -> u16 {
        self.timestamp_millis_part
    }

    /// Encodes the transaction calldata using standard ABI encoding.
    pub fn encode_calldata(&self) -> Bytes {
        let mut calldata = Vec::with_capacity(Self::CALLDATA_LEN);
        calldata.extend_from_slice(&Self::SELECTOR);
        calldata.extend_from_slice(&[0; 30]);
        calldata.extend_from_slice(&self.timestamp_millis_part.to_be_bytes());
        calldata.into()
    }

    /// Decodes a [`BaseTimeUpdateTx`] from standard ABI calldata.
    pub fn decode_calldata(calldata: &[u8]) -> Result<Self, BaseTimeUpdateDecodeError> {
        if calldata.len() < 4 {
            return Err(BaseTimeUpdateDecodeError::MissingSelector);
        }
        if calldata[..4] != Self::SELECTOR {
            return Err(BaseTimeUpdateDecodeError::InvalidSelector);
        }
        if calldata.len() != Self::CALLDATA_LEN {
            return Err(BaseTimeUpdateDecodeError::InvalidLength(
                Self::CALLDATA_LEN,
                calldata.len(),
            ));
        }
        if calldata[4..34].iter().any(|byte| *byte != 0) {
            return Err(BaseTimeUpdateDecodeError::NonZeroPadding);
        }

        let timestamp_millis_part = u16::from_be_bytes([calldata[34], calldata[35]]);
        Self::new(timestamp_millis_part)
            .map_err(BaseTimeUpdateDecodeError::InvalidTimestampMillisPart)
    }

    /// Extracts and validates the `BaseTime` metadata deposit at `tx[1]`.
    pub fn extract_from_transactions<T: BaseTransaction>(
        transactions: &[T],
        block_number: u64,
    ) -> Result<Self, BaseTimeMetadataError> {
        let transaction = transactions.get(1).ok_or(BaseTimeMetadataError::Missing)?;
        let deposit = transaction.as_deposit().ok_or(BaseTimeMetadataError::NotDeposit)?;
        let base_time = Self::validate_deposit(deposit, block_number)?;

        Ok(base_time)
    }

    /// Returns whether a transaction is a protocol-authorized `BaseTime` setter call.
    pub fn is_protocol_authorized_setter<T: BaseTransaction>(transaction: &T) -> bool {
        transaction.as_deposit().is_some_and(|deposit| {
            deposit.from == SystemAddresses::DEPOSITOR_ACCOUNT
                && deposit.to == TxKind::Call(Predeploys::BASE_TIME)
                && deposit.input.starts_with(&Self::SELECTOR)
        })
    }

    /// Validates the `BaseTime` transactions for a child block.
    ///
    /// Before Denim, protocol-authorized setter deposits are forbidden. At and after Denim, the
    /// canonical metadata deposit must be at `tx[1]`, and no other protocol-authorized setter may
    /// appear in the block.
    pub fn validate_child_transactions<T: BaseTransaction>(
        transactions: &[T],
        block_number: u64,
        denim_active: bool,
    ) -> Result<Option<Self>, BaseTimeValidationError> {
        if !denim_active {
            if let Some(index) = transactions.iter().position(Self::is_protocol_authorized_setter) {
                return Err(BaseTimeValidationError::ProtocolSetterBeforeDenim { index });
            }
            return Ok(None);
        }

        Self::validate_denim_child_transactions(transactions, block_number).map(Some)
    }

    /// Validates the canonical metadata and unique protocol writer for a Denim child block.
    pub fn validate_denim_child_transactions<T: BaseTransaction>(
        transactions: &[T],
        block_number: u64,
    ) -> Result<Self, BaseTimeValidationError> {
        let base_time = Self::extract_from_transactions(transactions, block_number)?;
        if let Some(index) = transactions.iter().enumerate().find_map(|(index, transaction)| {
            (index != 1 && Self::is_protocol_authorized_setter(transaction)).then_some(index)
        }) {
            return Err(BaseTimeValidationError::AdditionalProtocolSetter { index });
        }

        Ok(base_time)
    }

    /// Validates this update against the configured child timestamp.
    pub const fn validate_scheduled_timestamp(
        &self,
        block_timestamp: u64,
        expected_timestamp: u64,
        expected_timestamp_millis_part: u16,
    ) -> Result<(), BaseTimeValidationError> {
        if block_timestamp != expected_timestamp
            || self.timestamp_millis_part != expected_timestamp_millis_part
        {
            return Err(BaseTimeValidationError::ScheduledTimestampMismatch {
                expected_timestamp_ms: expected_timestamp as u128 * 1_000
                    + expected_timestamp_millis_part as u128,
                actual_timestamp_ms: block_timestamp as u128 * 1_000
                    + self.timestamp_millis_part as u128,
            });
        }

        Ok(())
    }

    /// Validates that the first Denim block starts at the `.000` lattice point.
    pub const fn validate_first_denim_anchor(&self) -> Result<(), BaseTimeValidationError> {
        if self.timestamp_millis_part != 0 {
            return Err(BaseTimeValidationError::InvalidFirstDenimAnchor {
                timestamp_millis_part: self.timestamp_millis_part,
            });
        }

        Ok(())
    }

    /// Validates that this child advances exactly one `BaseTime` slot from an active parent.
    pub const fn validate_progression(
        &self,
        parent_timestamp: u64,
        parent_timestamp_millis_part: u16,
        child_timestamp: u64,
    ) -> Result<(), BaseTimeValidationError> {
        let parent_timestamp_ms =
            parent_timestamp as u128 * 1_000 + parent_timestamp_millis_part as u128;
        let child_timestamp_ms =
            child_timestamp as u128 * 1_000 + self.timestamp_millis_part as u128;
        if child_timestamp_ms != parent_timestamp_ms + Self::BLOCK_INTERVAL_MILLIS as u128 {
            return Err(BaseTimeValidationError::ProgressionMismatch {
                parent_timestamp_ms,
                child_timestamp_ms,
            });
        }

        Ok(())
    }

    /// Validates receipt cardinality and successful execution of the metadata transaction.
    pub fn validate_receipts<R: TxReceipt>(
        transaction_count: usize,
        receipts: &[R],
    ) -> Result<(), BaseTimeValidationError> {
        if receipts.len() != transaction_count {
            return Err(BaseTimeValidationError::ReceiptCountMismatch {
                transaction_count,
                receipt_count: receipts.len(),
            });
        }
        let receipt = receipts.get(1).ok_or(BaseTimeValidationError::MetadataReceiptMissing)?;

        if !receipt.status() {
            return Err(BaseTimeValidationError::MetadataExecutionFailed);
        }

        Ok(())
    }

    /// Validates the final child `BaseTime` storage value.
    pub const fn validate_final_state(
        &self,
        timestamp_millis_part: u16,
    ) -> Result<(), BaseTimeValidationError> {
        if timestamp_millis_part != self.timestamp_millis_part {
            return Err(BaseTimeValidationError::FinalStateMismatch {
                expected_timestamp_millis_part: self.timestamp_millis_part,
                actual_timestamp_millis_part: timestamp_millis_part,
            });
        }

        Ok(())
    }

    /// Extracts the block timestamp in milliseconds from its transactions and header fields.
    pub fn extract_timestamp_ms<T: BaseTransaction>(
        transactions: &[T],
        block_number: u64,
        timestamp: u64,
    ) -> Result<u64, BaseTimeMetadataError> {
        let base_time = Self::extract_from_transactions(transactions, block_number)?;
        Ok(timestamp.wrapping_mul(1_000).wrapping_add(u64::from(base_time.timestamp_millis_part())))
    }

    /// Validates and decodes a `BaseTime` metadata deposit.
    pub fn validate_deposit(
        deposit: &TxDeposit,
        block_number: u64,
    ) -> Result<Self, BaseTimeMetadataError> {
        let expected_source_hash =
            DepositSourceDomain::BaseTime(BaseTimeDepositSource { block_number }).source_hash();
        if deposit.source_hash != expected_source_hash {
            return Err(BaseTimeMetadataError::InvalidSourceHash);
        }
        if deposit.from != SystemAddresses::DEPOSITOR_ACCOUNT {
            return Err(BaseTimeMetadataError::InvalidSender);
        }
        if deposit.to != TxKind::Call(Predeploys::BASE_TIME) {
            return Err(BaseTimeMetadataError::InvalidRecipient);
        }
        if deposit.mint != 0 {
            return Err(BaseTimeMetadataError::NonZeroMint);
        }
        if deposit.value != U256::ZERO {
            return Err(BaseTimeMetadataError::NonZeroValue);
        }
        if deposit.gas_limit != REGOLITH_SYSTEM_TX_GAS {
            return Err(BaseTimeMetadataError::InvalidGasLimit);
        }
        if deposit.is_system_transaction {
            return Err(BaseTimeMetadataError::SystemTransaction);
        }

        Self::decode_calldata(&deposit.input).map_err(BaseTimeMetadataError::InvalidCalldata)
    }

    /// Converts this update into a typed deposit transaction for inclusion at `tx[1]`.
    ///
    /// Callers are responsible for activation gating.
    pub fn into_deposit_tx(self, l2_block_number: u64) -> Sealed<TxDeposit> {
        let source =
            DepositSourceDomain::BaseTime(BaseTimeDepositSource { block_number: l2_block_number });

        let deposit_tx = TxDeposit {
            source_hash: source.source_hash(),
            from: SystemAddresses::DEPOSITOR_ACCOUNT,
            to: TxKind::Call(Predeploys::BASE_TIME),
            mint: 0,
            value: U256::ZERO,
            // BaseTime only activates on post-Regolith chains, so this deposit always uses the
            // ordinary-deposit semantics introduced there.
            gas_limit: REGOLITH_SYSTEM_TX_GAS,
            is_system_transaction: false,
            input: self.encode_calldata(),
        };

        deposit_tx.seal_slow()
    }
}

/// An error building a `BaseTime` metadata deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseTimeUpdateError {
    /// The millis part is not aligned to 200ms slots.
    #[error("invalid BaseTime timestamp millis part: {0}")]
    InvalidTimestampMillisPart(u16),
}

/// An error decoding `BaseTime` metadata calldata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseTimeUpdateDecodeError {
    /// The calldata is shorter than the selector.
    #[error("the provided calldata is too short, missing the 4 selector bytes")]
    MissingSelector,
    /// The selector does not match the `BaseTime` setter ABI.
    #[error("invalid BaseTime transaction selector")]
    InvalidSelector,
    /// The calldata length does not match the ABI shape.
    #[error("invalid BaseTime calldata length. Expected {0}, got {1}")]
    InvalidLength(usize, usize),
    /// The ABI padding for the `uint16` argument must be zero.
    #[error("invalid BaseTime calldata padding")]
    NonZeroPadding,
    /// The encoded millis part is not aligned to 200ms slots.
    #[error("invalid BaseTime timestamp millis part: {0}")]
    InvalidTimestampMillisPart(BaseTimeUpdateError),
}

/// An error extracting or validating a `BaseTime` metadata deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseTimeMetadataError {
    /// The block does not contain a transaction at `tx[1]`.
    #[error("missing BaseTime metadata deposit at tx[1]")]
    Missing,
    /// The transaction at `tx[1]` is not a deposit transaction.
    #[error("BaseTime metadata transaction is not a deposit")]
    NotDeposit,
    /// The deposit source hash does not commit to the block number and `BaseTime` domain.
    #[error("invalid BaseTime metadata source hash")]
    InvalidSourceHash,
    /// The deposit sender is not the protocol depositor.
    #[error("invalid BaseTime metadata sender")]
    InvalidSender,
    /// The deposit does not call the `BaseTime` predeploy.
    #[error("invalid BaseTime metadata recipient")]
    InvalidRecipient,
    /// The deposit mints ETH.
    #[error("BaseTime metadata deposit has non-zero mint")]
    NonZeroMint,
    /// The deposit transfers ETH.
    #[error("BaseTime metadata deposit has non-zero value")]
    NonZeroValue,
    /// The deposit gas limit does not match the protocol constant.
    #[error("invalid BaseTime metadata gas limit")]
    InvalidGasLimit,
    /// The deposit uses pre-Regolith system-transaction semantics.
    #[error("BaseTime metadata deposit is a system transaction")]
    SystemTransaction,
    /// The deposit calldata is not a canonical `BaseTime` setter call.
    #[error("invalid BaseTime metadata calldata: {0}")]
    InvalidCalldata(BaseTimeUpdateDecodeError),
}

/// An error validating a complete `BaseTime` child-state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseTimeValidationError {
    /// The canonical metadata deposit is malformed or missing.
    #[error(transparent)]
    Metadata(#[from] BaseTimeMetadataError),
    /// A protocol-authorized setter deposit appeared before Denim activation.
    #[error("protocol-authorized BaseTime setter at tx[{index}] before Denim")]
    ProtocolSetterBeforeDenim {
        /// The transaction index.
        index: usize,
    },
    /// A protocol-authorized setter appeared outside the canonical `tx[1]` position.
    #[error("additional protocol-authorized BaseTime setter at tx[{index}]")]
    AdditionalProtocolSetter {
        /// The transaction index.
        index: usize,
    },
    /// The child header and metadata do not match the configured schedule.
    #[error(
        "BaseTime scheduled timestamp mismatch: expected {expected_timestamp_ms}ms, got {actual_timestamp_ms}ms"
    )]
    ScheduledTimestampMismatch {
        /// The configured child timestamp in milliseconds.
        expected_timestamp_ms: u128,
        /// The child header and metadata timestamp in milliseconds.
        actual_timestamp_ms: u128,
    },
    /// The first Denim block does not start at `.000`.
    #[error("invalid first Denim BaseTime anchor: expected 0ms, got {timestamp_millis_part}ms")]
    InvalidFirstDenimAnchor {
        /// The metadata millisecond component.
        timestamp_millis_part: u16,
    },
    /// Consecutive Denim blocks did not advance by exactly one 200ms slot.
    #[error(
        "invalid BaseTime progression from {parent_timestamp_ms}ms to {child_timestamp_ms}ms; expected exactly 200ms"
    )]
    ProgressionMismatch {
        /// The parent's full-millisecond timestamp.
        parent_timestamp_ms: u128,
        /// The child's full-millisecond timestamp.
        child_timestamp_ms: u128,
    },
    /// Execution produced a different number of receipts than transactions.
    #[error(
        "BaseTime receipt count mismatch: {transaction_count} transactions, {receipt_count} receipts"
    )]
    ReceiptCountMismatch {
        /// The block transaction count.
        transaction_count: usize,
        /// The execution receipt count.
        receipt_count: usize,
    },
    /// Execution did not produce the metadata receipt at `tx[1]`.
    #[error("missing BaseTime metadata receipt at tx[1]")]
    MetadataReceiptMissing,
    /// The metadata transaction reverted.
    #[error("BaseTime metadata transaction execution failed")]
    MetadataExecutionFailed,
    /// The final `BaseTime` slot does not match the metadata value.
    #[error(
        "BaseTime final state mismatch: expected {expected_timestamp_millis_part}ms, got {actual_timestamp_millis_part}ms"
    )]
    FinalStateMismatch {
        /// The millisecond component committed by `tx[1]`.
        expected_timestamp_millis_part: u16,
        /// The millisecond component in final child state.
        actual_timestamp_millis_part: u16,
    },
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Receipt, Sealable, TxLegacy};
    use alloy_primitives::{Address, B256, Signature, TxKind, U256};
    use base_common_consensus::{
        BaseTransactionSigned, BaseTypedTransaction, Predeploys, SystemAddresses, TxDeposit,
    };

    use super::{
        BaseTimeMetadataError, BaseTimeUpdateDecodeError, BaseTimeUpdateError, BaseTimeUpdateTx,
        BaseTimeValidationError,
    };
    use crate::REGOLITH_SYSTEM_TX_GAS;

    fn base_time_deposit(block_number: u64, timestamp_millis_part: u16) -> TxDeposit {
        BaseTimeUpdateTx::new(timestamp_millis_part)
            .unwrap()
            .into_deposit_tx(block_number)
            .into_inner()
    }

    fn user_transaction() -> BaseTransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };
        BaseTransactionSigned::new_unhashed(
            BaseTypedTransaction::Legacy(tx),
            Signature::new(U256::ZERO, U256::ZERO, false),
        )
    }

    fn user_base_time_transaction(timestamp_millis_part: u16) -> BaseTransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Predeploys::BASE_TIME),
            value: U256::ZERO,
            input: BaseTimeUpdateTx::new(timestamp_millis_part).unwrap().encode_calldata(),
        };
        BaseTransactionSigned::new_unhashed(
            BaseTypedTransaction::Legacy(tx),
            Signature::new(U256::ZERO, U256::ZERO, false),
        )
    }

    fn receipt(status: bool) -> Receipt {
        Receipt { status: status.into(), cumulative_gas_used: 1, logs: vec![] }
    }

    #[test]
    fn base_time_update_roundtrips() {
        let base_time = BaseTimeUpdateTx::new(400).unwrap();
        let decoded = BaseTimeUpdateTx::decode_calldata(&base_time.encode_calldata()).unwrap();

        assert_eq!(decoded, base_time);
    }

    #[test]
    fn base_time_update_rejects_invalid_millis_part() {
        assert_eq!(
            BaseTimeUpdateTx::new(100),
            Err(BaseTimeUpdateError::InvalidTimestampMillisPart(100))
        );
    }

    #[test]
    fn validates_timestamp_millis_lattice() {
        for value in [0, 200, 400, 600, 800] {
            assert!(BaseTimeUpdateTx::is_valid_timestamp_millis_part(value));
        }

        for value in [1, 100, 199, 201, 999, 1_000] {
            assert!(!BaseTimeUpdateTx::is_valid_timestamp_millis_part(value));
        }
    }

    #[test]
    fn base_time_update_rejects_non_zero_padding() {
        let mut calldata = BaseTimeUpdateTx::new(200).unwrap().encode_calldata().to_vec();
        calldata[4] = 1;

        assert_eq!(
            BaseTimeUpdateTx::decode_calldata(&calldata),
            Err(BaseTimeUpdateDecodeError::NonZeroPadding)
        );
    }

    #[test]
    fn base_time_update_builds_deposit_tx() {
        let l2_block_number = 9;
        let base_time = BaseTimeUpdateTx::new(600).unwrap();
        let deposit_tx = base_time.into_deposit_tx(l2_block_number);

        assert_eq!(deposit_tx.from, SystemAddresses::DEPOSITOR_ACCOUNT);
        assert_eq!(deposit_tx.to, TxKind::Call(Predeploys::BASE_TIME));
        assert_eq!(deposit_tx.mint, 0);
        assert_eq!(deposit_tx.value, U256::ZERO);
        assert_eq!(deposit_tx.gas_limit, REGOLITH_SYSTEM_TX_GAS);
        assert!(!deposit_tx.is_system_transaction);
        assert_eq!(deposit_tx.input, base_time.encode_calldata());
    }

    #[test]
    fn extracts_valid_base_time_metadata_at_tx_one() {
        let transactions: Vec<BaseTransactionSigned> = vec![
            TxDeposit::default().seal_slow().into(),
            base_time_deposit(9, 600).seal_slow().into(),
        ];

        let base_time = BaseTimeUpdateTx::extract_from_transactions(&transactions, 9).unwrap();

        assert_eq!(base_time.timestamp_millis_part(), 600);
    }

    #[test]
    fn validates_child_transaction_activation_and_unique_writer() {
        let legacy_transactions: Vec<BaseTransactionSigned> =
            vec![TxDeposit::default().seal_slow().into()];
        assert_eq!(
            BaseTimeUpdateTx::validate_child_transactions(&legacy_transactions, 8, false),
            Ok(None)
        );

        let canonical_transactions: Vec<BaseTransactionSigned> = vec![
            TxDeposit::default().seal_slow().into(),
            base_time_deposit(9, 0).seal_slow().into(),
            user_base_time_transaction(200),
        ];
        assert_eq!(
            BaseTimeUpdateTx::validate_child_transactions(&canonical_transactions, 9, true),
            Ok(Some(BaseTimeUpdateTx::new(0).unwrap()))
        );
        assert_eq!(
            BaseTimeUpdateTx::validate_child_transactions(&canonical_transactions, 9, false),
            Err(BaseTimeValidationError::ProtocolSetterBeforeDenim { index: 1 })
        );

        for duplicate_millis_part in [0, 200] {
            let transactions: Vec<BaseTransactionSigned> = vec![
                TxDeposit::default().seal_slow().into(),
                base_time_deposit(9, 0).seal_slow().into(),
                base_time_deposit(9, duplicate_millis_part).seal_slow().into(),
            ];
            assert_eq!(
                BaseTimeUpdateTx::validate_child_transactions(&transactions, 9, true),
                Err(BaseTimeValidationError::AdditionalProtocolSetter { index: 2 })
            );
        }
    }

    #[test]
    fn validates_child_schedule_anchor_and_progression() {
        let first = BaseTimeUpdateTx::new(0).unwrap();
        first.validate_scheduled_timestamp(42, 42, 0).unwrap();
        first.validate_first_denim_anchor().unwrap();
        assert_eq!(
            first.validate_scheduled_timestamp(42, 42, 200),
            Err(BaseTimeValidationError::ScheduledTimestampMismatch {
                expected_timestamp_ms: 42_200,
                actual_timestamp_ms: 42_000,
            })
        );

        assert_eq!(
            BaseTimeUpdateTx::new(200).unwrap().validate_first_denim_anchor(),
            Err(BaseTimeValidationError::InvalidFirstDenimAnchor { timestamp_millis_part: 200 })
        );

        for (parent_seconds, parent_millis, child_seconds, child_millis) in [
            (42, 0, 42, 200),
            (42, 200, 42, 400),
            (42, 400, 42, 600),
            (42, 600, 42, 800),
            (42, 800, 43, 0),
        ] {
            BaseTimeUpdateTx::new(child_millis)
                .unwrap()
                .validate_progression(parent_seconds, parent_millis, child_seconds)
                .unwrap();
        }

        assert_eq!(
            BaseTimeUpdateTx::new(600).unwrap().validate_progression(42, 200, 42),
            Err(BaseTimeValidationError::ProgressionMismatch {
                parent_timestamp_ms: 42_200,
                child_timestamp_ms: 42_600,
            })
        );
    }

    #[test]
    fn validates_metadata_receipt_and_final_state() {
        let update = BaseTimeUpdateTx::new(400).unwrap();
        update.validate_final_state(400).unwrap();
        assert_eq!(
            update.validate_final_state(200),
            Err(BaseTimeValidationError::FinalStateMismatch {
                expected_timestamp_millis_part: 400,
                actual_timestamp_millis_part: 200,
            })
        );

        BaseTimeUpdateTx::validate_receipts(2, &[receipt(true), receipt(true)]).unwrap();
        assert_eq!(
            BaseTimeUpdateTx::validate_receipts(2, &[receipt(true)]),
            Err(BaseTimeValidationError::ReceiptCountMismatch {
                transaction_count: 2,
                receipt_count: 1,
            })
        );
        assert_eq!(
            BaseTimeUpdateTx::validate_receipts(1, &[receipt(true)]),
            Err(BaseTimeValidationError::MetadataReceiptMissing)
        );
        assert_eq!(
            BaseTimeUpdateTx::validate_receipts(
                3,
                &[receipt(true), receipt(true), receipt(true), receipt(true)]
            ),
            Err(BaseTimeValidationError::ReceiptCountMismatch {
                transaction_count: 3,
                receipt_count: 4,
            })
        );
        assert_eq!(
            BaseTimeUpdateTx::validate_receipts(2, &[receipt(true), receipt(false)]),
            Err(BaseTimeValidationError::MetadataExecutionFailed)
        );
    }

    #[test]
    fn extracts_timestamp_ms() {
        let transactions: Vec<BaseTransactionSigned> = vec![
            TxDeposit::default().seal_slow().into(),
            base_time_deposit(9, 600).seal_slow().into(),
        ];

        assert_eq!(BaseTimeUpdateTx::extract_timestamp_ms(&transactions, 9, 42), Ok(42_600));
    }

    #[test]
    fn rejects_missing_or_mispositioned_base_time_metadata() {
        let l1_info: BaseTransactionSigned = TxDeposit::default().seal_slow().into();
        assert_eq!(
            BaseTimeUpdateTx::extract_from_transactions(&[l1_info], 9),
            Err(BaseTimeMetadataError::Missing)
        );

        let transactions = vec![
            TxDeposit::default().seal_slow().into(),
            user_transaction(),
            base_time_deposit(9, 600).seal_slow().into(),
        ];
        assert_eq!(
            BaseTimeUpdateTx::extract_from_transactions(&transactions, 9),
            Err(BaseTimeMetadataError::NotDeposit)
        );
    }

    #[test]
    fn rejects_invalid_base_time_deposit_envelope() {
        let mut deposit = base_time_deposit(9, 600);
        deposit.source_hash = B256::ZERO;
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidSourceHash)
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.from = Address::ZERO;
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidSender)
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.to = TxKind::Call(Address::ZERO);
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidRecipient)
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.mint = 1;
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::NonZeroMint)
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.value = U256::from(1);
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::NonZeroValue)
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.gas_limit -= 1;
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidGasLimit)
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.is_system_transaction = true;
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::SystemTransaction)
        );
    }

    #[test]
    fn rejects_invalid_base_time_deposit_calldata() {
        let mut deposit = base_time_deposit(9, 600);
        let mut input = deposit.input.to_vec();
        input[0] ^= 0xff;
        deposit.input = input.into();
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidCalldata(BaseTimeUpdateDecodeError::InvalidSelector))
        );

        let mut deposit = base_time_deposit(9, 600);
        let mut input = deposit.input.to_vec();
        input[34..].copy_from_slice(&100_u16.to_be_bytes());
        deposit.input = input.into();
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidCalldata(
                BaseTimeUpdateDecodeError::InvalidTimestampMillisPart(
                    BaseTimeUpdateError::InvalidTimestampMillisPart(100)
                )
            ))
        );

        let mut deposit = base_time_deposit(9, 600);
        deposit.input = deposit.input[..3].to_vec().into();
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidCalldata(BaseTimeUpdateDecodeError::MissingSelector))
        );

        let mut deposit = base_time_deposit(9, 600);
        let mut input = deposit.input.to_vec();
        input.push(0);
        deposit.input = input.into();
        assert_eq!(
            BaseTimeUpdateTx::validate_deposit(&deposit, 9),
            Err(BaseTimeMetadataError::InvalidCalldata(BaseTimeUpdateDecodeError::InvalidLength(
                36, 37
            )))
        );
    }
}
