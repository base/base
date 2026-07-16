//! `BaseTime` metadata deposit transaction encoding.

use alloc::vec::Vec;

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

#[cfg(test)]
mod tests {
    use alloy_consensus::{Sealable, TxLegacy};
    use alloy_primitives::{Address, B256, Signature, TxKind, U256};
    use base_common_consensus::{
        BaseTransactionSigned, BaseTypedTransaction, Predeploys, SystemAddresses, TxDeposit,
    };

    use super::{
        BaseTimeMetadataError, BaseTimeUpdateDecodeError, BaseTimeUpdateError, BaseTimeUpdateTx,
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
