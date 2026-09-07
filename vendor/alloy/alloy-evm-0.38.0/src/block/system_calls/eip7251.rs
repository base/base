//! [EIP-7251](https://eips.ethereum.org/EIPS/eip-7251) system call implementation.

use crate::{
    block::{BlockExecutionError, BlockValidationError},
    Evm,
};
use alloc::format;
use alloy_eips::eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS;
use alloy_primitives::Bytes;
use core::fmt::Debug;
use revm::context_interface::result::{ExecutionResult, ResultAndState};

/// Applies the post-block call to the EIP-7251 consolidation requests contract.
///
/// If Prague is not active at the given timestamp, then this is a no-op, and an empty vector is
/// returned. Otherwise, the consolidation requests are returned.
///
/// Note: this does not commit the state changes to the database, it only transacts the call.
#[inline]
pub(crate) fn transact_consolidation_requests_contract_call<Halt>(
    evm: &mut impl Evm<HaltReason = Halt>,
) -> Result<ResultAndState<Halt>, BlockExecutionError> {
    // Execute EIP-7251 consolidation requests contract call.
    //
    // The requirement for the consolidation requests contract call defined by
    // [EIP-7251](https://eips.ethereum.org/EIPS/eip-7251) is:
    //
    // At the end of processing any execution block where block.timestamp >= FORK_TIMESTAMP (i.e.
    // after processing all transactions and after performing the block body requests validations)
    // client software MUST call the contract as `SYSTEM_ADDRESS` with empty input data to
    // trigger the system subroutine execution.
    let res = match evm.transact_system_call(
        alloy_eips::eip7002::SYSTEM_ADDRESS,
        CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
        Bytes::new(),
    ) {
        Ok(res) => res,
        Err(e) => {
            return Err(BlockValidationError::ConsolidationRequestsContractCall {
                message: format!("execution failed: {e}"),
            }
            .into())
        }
    };

    Ok(res)
}

/// Calls the consolidation requests system contract, and returns the requests from the execution
/// output.
#[inline]
pub(crate) fn post_commit<Halt: Debug>(
    result: ExecutionResult<Halt>,
) -> Result<Bytes, BlockExecutionError> {
    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        ExecutionResult::Revert { output, .. } => {
            Err(BlockValidationError::ConsolidationRequestsContractCall {
                message: format!("execution reverted: {output}"),
            }
            .into())
        }
        ExecutionResult::Halt { reason, .. } => {
            Err(BlockValidationError::ConsolidationRequestsContractCall {
                message: format!("execution halted: {reason:?}"),
            }
            .into())
        }
    }
}
