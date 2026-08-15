//! `BaseTime` metadata deposit transaction encoding.

use alloc::vec::Vec;

use alloy_primitives::{Bytes, Sealable, Sealed, TxKind, U256};
use base_common_consensus::{
    BaseTimeDepositSource, DepositSourceDomain, Predeploys, SystemAddresses, TxDeposit,
};
use base_common_genesis::RollupConfig;

use crate::REGOLITH_SYSTEM_TX_GAS;

/// Versioned calldata for the `BaseTime` metadata deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BaseTimeUpdateTx {
    /// The sub-second millisecond component for the block timestamp.
    timestamp_millis_part: u16,
}

impl BaseTimeUpdateTx {
    /// The selector for `setTimestampMillisPart(uint16)`.
    pub const SELECTOR: [u8; 4] = [0x86, 0xbd, 0xf3, 0x94];

    /// The ABI calldata length.
    pub const CALLDATA_LEN: usize = 4 + 32;

    /// Creates a new [`BaseTimeUpdateTx`].
    pub const fn new(timestamp_millis_part: u16) -> Result<Self, BaseTimeUpdateError> {
        if !matches!(timestamp_millis_part, 0 | 200 | 400 | 600 | 800) {
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

    /// Returns a typed deposit transaction for inclusion at `tx[1]`.
    ///
    /// Callers are responsible for activation gating; this helper only validates and encodes the
    /// `BaseTime` metadata deposit once the surrounding protocol has decided it is allowed.
    ///
    /// `_l2_parent_block_time` is reserved so later hard-fork activation logic can make the same
    /// same-second boundary decisions here that [`crate::L1BlockInfoTx::try_new_with_deposit_tx`]
    /// already needs.
    pub fn try_new_with_deposit_tx(
        _rollup_config: &RollupConfig,
        l2_block_number: u64,
        timestamp_millis_part: u16,
        _l2_parent_block_time: u64,
        _l2_block_time: u64,
    ) -> Result<(Self, Sealed<TxDeposit>), BaseTimeUpdateError> {
        let base_time = Self::new(timestamp_millis_part)?;
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
            input: base_time.encode_calldata(),
        };

        Ok((base_time, deposit_tx.seal_slow()))
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{TxKind, U256};
    use base_common_consensus::{Predeploys, SystemAddresses};
    use base_common_genesis::RollupConfig;

    use super::{BaseTimeUpdateDecodeError, BaseTimeUpdateError, BaseTimeUpdateTx};
    use crate::REGOLITH_SYSTEM_TX_GAS;

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
        let rollup_config = RollupConfig::default();
        let l2_block_number = 9;

        let (base_time, deposit_tx) =
            BaseTimeUpdateTx::try_new_with_deposit_tx(&rollup_config, l2_block_number, 600, 1, 2)
                .unwrap();

        assert_eq!(deposit_tx.from, SystemAddresses::DEPOSITOR_ACCOUNT);
        assert_eq!(deposit_tx.to, TxKind::Call(Predeploys::BASE_TIME));
        assert_eq!(deposit_tx.mint, 0);
        assert_eq!(deposit_tx.value, U256::ZERO);
        assert_eq!(deposit_tx.gas_limit, REGOLITH_SYSTEM_TX_GAS);
        assert!(!deposit_tx.is_system_transaction);
        assert_eq!(deposit_tx.input, base_time.encode_calldata());
    }
}
