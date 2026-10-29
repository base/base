//! Denim `BaseTime` predeploy install transition.

use alloy_primitives::{Address, B256, Bytes, U256, address, b256, hex, uint};
use base_common_consensus::Predeploys;
use base_common_genesis::BaseUpgrade;
use evm2::{
    AccountInfo, Evm,
    bytecode::Bytecode,
    registry::{HandlerError, HandlerResult},
};

use crate::{BaseBlockState, BaseEvmTypes, BaseForkActivations, IrregularStateChange};

/// Errors produced while applying the `BaseTime` predeploy transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseTimeTransitionError {
    /// The reserved proxy account is missing.
    MissingProxy,
    /// The reserved proxy account does not contain code.
    CodelessProxy,
    /// The reserved proxy does not have the canonical admin.
    UnexpectedProxyAdmin {
        /// The admin slot value found in state.
        actual: U256,
    },
}

impl core::fmt::Display for BaseTimeTransitionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingProxy => {
                f.write_str("BaseTime activation requires the reserved proxy account")
            }
            Self::CodelessProxy => f.write_str("BaseTime activation requires existing proxy code"),
            Self::UnexpectedProxyAdmin { actual } => {
                write!(
                    f,
                    "BaseTime activation requires the canonical proxy admin, found {actual:#x}"
                )
            }
        }
    }
}

impl core::error::Error for BaseTimeTransitionError {}

/// The Denim `BaseTime` predeploy transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseTime;

impl BaseTime {
    /// The code-namespace address used by the canonical `BaseTime` proxy.
    pub const IMPLEMENTATION_ADDRESS: Address =
        address!("0xc0D3C0d3C0d3C0D3c0d3C0d3c0D3C0d3c0d30030");

    /// The expected hash of the canonical `BaseTime` runtime bytecode.
    pub const IMPLEMENTATION_CODE_HASH: B256 =
        b256!("0x9c4c8a497a69d0b8f2ba67be0bee7a1373186055978c3be6ec3068e0ec27f32a");

    /// The expected hash of the canonical predeploy proxy runtime bytecode.
    pub const PROXY_CODE_HASH: B256 =
        b256!("0xfa8c9db6c6cab7108dea276f4cd09d575674eb0852c0fa3187e59e98ef977998");

    /// The selector of `setTimestampMillisPart(uint16)`.
    pub const SET_TIMESTAMP_MILLIS_PART_SELECTOR: [u8; 4] = hex!("86bdf394");

    /// The selector of `timestampMillisPart()`.
    pub const TIMESTAMP_MILLIS_PART_SELECTOR: [u8; 4] = hex!("7b2fea99");

    /// The selector of `timestampMs()`.
    pub const TIMESTAMP_MS_SELECTOR: [u8; 4] = hex!("5745a677");

    /// The slot that stores the `timestamp_millis_part` value in its low `uint16`.
    pub const TIMESTAMP_MILLIS_PART_SLOT: U256 = uint!(0_U256);

    /// The EIP-1967 proxy implementation slot.
    pub const IMPLEMENTATION_SLOT: U256 =
        uint!(0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc_U256);

    /// The EIP-1967 proxy admin slot.
    pub const ADMIN_SLOT: U256 =
        uint!(0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103_U256);

    /// Returns the canonical `BaseTime` runtime bytecode from `base/contracts` commit `4848ec70`.
    pub fn implementation_bytecode() -> Bytes {
        hex::decode(include_str!("bytecode/base-time.hex").trim())
            .expect("BaseTime runtime artifact must be valid hex")
            .into()
    }

    /// Returns the canonical predeploy proxy runtime from `base/contracts` commit `4848ec70`.
    pub fn proxy_bytecode() -> Bytes {
        hex::decode(include_str!("bytecode/proxy.hex").trim())
            .expect("proxy runtime artifact must be valid hex")
            .into()
    }

    /// Returns the block's Unix-millisecond timestamp: `seconds * 1000` plus the millisecond part
    /// the predeploy stores in the low `uint16` of
    /// [`TIMESTAMP_MILLIS_PART_SLOT`](Self::TIMESTAMP_MILLIS_PART_SLOT), wrapping like the
    /// predeploy's Solidity `uint64` arithmetic. The slot is read untracked so the transaction
    /// does not warm it.
    pub fn timestamp_ms(evm: &mut Evm<'_, BaseEvmTypes>, seconds: u64) -> HandlerResult<u64> {
        let slot = evm
            .state_mut()
            .storage_slot_untracked(&Predeploys::BASE_TIME, &Self::TIMESTAMP_MILLIS_PART_SLOT)?;
        let millis_part = slot.as_limbs()[0] & u64::from(u16::MAX);
        Ok(seconds.wrapping_mul(1_000).wrapping_add(millis_part))
    }

    /// Installs the `BaseTime` implementation and links its existing proxy before transactions.
    ///
    /// Existing chains must already contain the reserved proxy runtime with
    /// [`Predeploys::PROXY_ADMIN`] in its EIP-1967 admin slot. This transition validates that
    /// historical invariant; it does not install or repair proxy state.
    ///
    /// The transition is staged behind the `Denim` activation. The implementation slot is the
    /// durable migration marker: any existing linkage is preserved so later execution cannot
    /// rewrite the initial deployment or undo a governance upgrade.
    pub fn ensure_predeploy(
        chain_spec: &impl BaseForkActivations,
        timestamp: u64,
        evm: &mut Evm<'_, BaseEvmTypes>,
        block_state: &mut impl BaseBlockState,
    ) -> HandlerResult<()> {
        if !chain_spec.is_active_at_timestamp(BaseUpgrade::Denim, timestamp) {
            return Ok(());
        }

        let expected_implementation = U256::from_be_slice(Self::IMPLEMENTATION_ADDRESS.as_slice());
        let current_implementation = evm
            .state_mut()
            .storage_slot_untracked(&Predeploys::BASE_TIME, &Self::IMPLEMENTATION_SLOT)?;
        if current_implementation != U256::ZERO {
            return Ok(());
        }

        let proxy_info = evm
            .state_mut()
            .account_info_untracked(&Predeploys::BASE_TIME)?
            .ok_or_else(|| HandlerError::external(BaseTimeTransitionError::MissingProxy))?;
        if proxy_info.code_hash == alloy_primitives::KECCAK256_EMPTY {
            return Err(HandlerError::external(BaseTimeTransitionError::CodelessProxy));
        }

        let current_admin =
            evm.state_mut().storage_slot_untracked(&Predeploys::BASE_TIME, &Self::ADMIN_SLOT)?;
        if current_admin != U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()) {
            return Err(HandlerError::external(BaseTimeTransitionError::UnexpectedProxyAdmin {
                actual: current_admin,
            }));
        }

        // Install the implementation runtime at its code-namespace address.
        let code = Bytecode::new_raw(Self::implementation_bytecode());
        let code_hash = code.hash_slow();
        let implementation_original =
            evm.state_mut().account_info_untracked(&Self::IMPLEMENTATION_ADDRESS)?;
        let base = implementation_original.clone().unwrap_or_default();
        let implementation_current =
            AccountInfo::new(base.balance, base.nonce, code_hash, Some(code));
        IrregularStateChange::new(
            Self::IMPLEMENTATION_ADDRESS,
            implementation_original,
            Some(implementation_current),
        )
        .apply(evm, block_state);

        // Link the existing proxy's EIP-1967 implementation slot to the implementation address.
        IrregularStateChange::new(
            Predeploys::BASE_TIME,
            Some(proxy_info.clone()),
            Some(proxy_info),
        )
        .with_storage(Self::IMPLEMENTATION_SLOT, current_implementation, expected_implementation)
        .apply(evm, block_state);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    #[test]
    fn artifacts_match_canonical_contracts() {
        assert_eq!(
            keccak256(BaseTime::implementation_bytecode()),
            BaseTime::IMPLEMENTATION_CODE_HASH
        );
        assert_eq!(keccak256(BaseTime::proxy_bytecode()), BaseTime::PROXY_CODE_HASH);
    }

    #[test]
    fn selectors_match_solidity_signatures() {
        let selector =
            |signature: &str| -> [u8; 4] { keccak256(signature)[..4].try_into().unwrap() };
        assert_eq!(
            BaseTime::SET_TIMESTAMP_MILLIS_PART_SELECTOR,
            selector("setTimestampMillisPart(uint16)")
        );
        assert_eq!(BaseTime::TIMESTAMP_MILLIS_PART_SELECTOR, selector("timestampMillisPart()"));
        assert_eq!(BaseTime::TIMESTAMP_MS_SELECTOR, selector("timestampMs()"));
    }
}
