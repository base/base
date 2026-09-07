//! [EIP-8282](https://eips.ethereum.org/EIPS/eip-8282) builder execution requests system calls.
//!
//! EIP-8282 introduces two new EIP-7685 request types, processed by post-block system calls to
//! two predeploys, starting at the Amsterdam hardfork:
//!
//! * **Builder deposit requests** (request type `0x03`)
//! * **Builder exit requests** (request type `0x04`)
//!
//! Each predeploy maintains a FIFO request queue and an "excess" counter in storage slot 0 (the
//! same design as EIP-7002/7251). At the end of every block from Amsterdam onward the contract is
//! invoked as `SYSTEM_ADDRESS` with empty calldata, which drains the queue (returning the encoded
//! requests) and, on first invocation, initializes the excess counter from the
//! `EXCESS_INHIBITOR` sentinel (`2^256 - 1`) to `0`.

use crate::{
    block::{BlockExecutionError, BlockValidationError},
    Evm,
};
use alloc::format;
use alloy_eips::eip7002::SYSTEM_ADDRESS;
use alloy_primitives::{address, Address, Bytes};
use core::fmt::Debug;
use revm::context_interface::result::{ExecutionResult, ResultAndState};

/// The [EIP-7685](https://eips.ethereum.org/EIPS/eip-7685) request type for EIP-8282 builder
/// deposit requests.
pub const BUILDER_DEPOSIT_REQUEST_TYPE: u8 = 0x03;

/// The [EIP-7685](https://eips.ethereum.org/EIPS/eip-7685) request type for EIP-8282 builder exit
/// requests.
pub const BUILDER_EXIT_REQUEST_TYPE: u8 = 0x04;

/// The address of the EIP-8282 builder deposit requests predeploy.
pub const BUILDER_DEPOSIT_REQUEST_PREDEPLOY_ADDRESS: Address =
    address!("0x0000BFF46984E3725691FA540A8C7589300D8282");

/// The address of the EIP-8282 builder exit requests predeploy.
pub const BUILDER_EXIT_REQUEST_PREDEPLOY_ADDRESS: Address =
    address!("0x000064D678505AD48F8CCB093BC65613800E8282");

/// Applies the post-block call to the EIP-8282 builder deposit requests contract.
///
/// Note: this does not commit the state changes to the database, it only transacts the call.
#[inline]
pub(crate) fn transact_builder_deposit_requests_contract_call<Halt>(
    evm: &mut impl Evm<HaltReason = Halt>,
) -> Result<ResultAndState<Halt>, BlockExecutionError> {
    // At the end of processing any execution block where Amsterdam is active, call the builder
    // deposit requests contract as `SYSTEM_ADDRESS` with empty calldata.
    match evm.transact_system_call(
        SYSTEM_ADDRESS,
        BUILDER_DEPOSIT_REQUEST_PREDEPLOY_ADDRESS,
        Bytes::new(),
    ) {
        Ok(res) => Ok(res),
        Err(e) => Err(BlockValidationError::BuilderDepositRequestsContractCall {
            message: format!("execution failed: {e}"),
        }
        .into()),
    }
}

/// Applies the post-block call to the EIP-8282 builder exit requests contract.
///
/// Note: this does not commit the state changes to the database, it only transacts the call.
#[inline]
pub(crate) fn transact_builder_exit_requests_contract_call<Halt>(
    evm: &mut impl Evm<HaltReason = Halt>,
) -> Result<ResultAndState<Halt>, BlockExecutionError> {
    match evm.transact_system_call(
        SYSTEM_ADDRESS,
        BUILDER_EXIT_REQUEST_PREDEPLOY_ADDRESS,
        Bytes::new(),
    ) {
        Ok(res) => Ok(res),
        Err(e) => Err(BlockValidationError::BuilderExitRequestsContractCall {
            message: format!("execution failed: {e}"),
        }
        .into()),
    }
}

/// Extracts the builder deposit requests from the execution output.
#[inline]
pub(crate) fn deposit_post_commit<Halt: Debug>(
    result: ExecutionResult<Halt>,
) -> Result<Bytes, BlockExecutionError> {
    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        ExecutionResult::Revert { output, .. } => {
            Err(BlockValidationError::BuilderDepositRequestsContractCall {
                message: format!("execution reverted: {output}"),
            }
            .into())
        }
        ExecutionResult::Halt { reason, .. } => {
            Err(BlockValidationError::BuilderDepositRequestsContractCall {
                message: format!("execution halted: {reason:?}"),
            }
            .into())
        }
    }
}

/// Extracts the builder exit requests from the execution output.
#[inline]
pub(crate) fn exit_post_commit<Halt: Debug>(
    result: ExecutionResult<Halt>,
) -> Result<Bytes, BlockExecutionError> {
    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        ExecutionResult::Revert { output, .. } => {
            Err(BlockValidationError::BuilderExitRequestsContractCall {
                message: format!("execution reverted: {output}"),
            }
            .into())
        }
        ExecutionResult::Halt { reason, .. } => {
            Err(BlockValidationError::BuilderExitRequestsContractCall {
                message: format!("execution halted: {reason:?}"),
            }
            .into())
        }
    }
}
