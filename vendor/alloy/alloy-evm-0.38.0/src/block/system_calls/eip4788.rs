//! [EIP-4788](https://eips.ethereum.org/EIPS/eip-4788) system call implementation.

use crate::{
    block::{BlockExecutionError, BlockValidationError},
    Evm,
};
use alloc::{boxed::Box, string::ToString};
use alloy_eips::eip4788::BEACON_ROOTS_ADDRESS;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::B256;
use revm::{context::Block, context_interface::result::ResultAndState};

/// Applies the pre-block call to the [EIP-4788] beacon block root contract, using the given block,
/// chain spec, EVM.
///
/// Note: this does not commit the state changes to the database, it only transacts the call.
///
/// Returns `None` if Cancun is not active or the block is the genesis block, otherwise returns the
/// result of the call.
///
/// [EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
#[inline]
pub(crate) fn transact_beacon_root_contract_call<Halt>(
    spec: impl EthereumHardforks,
    parent_beacon_block_root: Option<B256>,
    evm: &mut impl Evm<HaltReason = Halt>,
) -> Result<Option<ResultAndState<Halt>>, BlockExecutionError> {
    if !spec.is_cancun_active_at_timestamp(evm.block().timestamp().saturating_to()) {
        return Ok(None);
    }

    let parent_beacon_block_root =
        parent_beacon_block_root.ok_or(BlockValidationError::MissingParentBeaconBlockRoot)?;

    // if the block number is zero (genesis block) then the parent beacon block root must
    // be 0x0 and no system transaction may occur as per EIP-4788
    if evm.block().number().is_zero() {
        if !parent_beacon_block_root.is_zero() {
            return Err(BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
                parent_beacon_block_root,
            }
            .into());
        }
        return Ok(None);
    }

    let res = match evm.transact_system_call(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        BEACON_ROOTS_ADDRESS,
        parent_beacon_block_root.0.into(),
    ) {
        Ok(res) => res,
        Err(e) => {
            return Err(BlockValidationError::BeaconRootContractCall {
                parent_beacon_block_root: Box::new(parent_beacon_block_root),
                message: e.to_string(),
            }
            .into())
        }
    };

    Ok(Some(res))
}
