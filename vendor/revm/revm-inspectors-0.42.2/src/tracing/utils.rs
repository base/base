//! Utility functions for revm related ops
use crate::tracing::config::TraceStyle;
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use alloy_primitives::{hex, Bytes};
use alloy_sol_types::{ContractError, GenericRevertReason};
use revm::{
    interpreter::InstructionResult,
    primitives::{hardfork::SpecId, KECCAK_EMPTY},
    DatabaseRef,
};

/// Converts a non successful [`InstructionResult`] to an error message.
///
/// Returns `None` if [`InstructionResult::is_ok`].
///
/// See also <https://github.com/ethereum/go-ethereum/blob/34d507215951fb3f4a5983b65e127577989a6db8/eth/tracers/native/call_flat.go#L39-L55>
pub(crate) fn fmt_error_msg(res: InstructionResult, kind: TraceStyle) -> Option<String> {
    if res.is_ok() {
        return None;
    }
    let msg = match res {
        InstructionResult::Revert => {
            if kind.is_parity() { "Reverted" } else { "execution reverted" }.to_string()
        }
        InstructionResult::OutOfGas | InstructionResult::PrecompileOOG => {
            if kind.is_parity() { "Out of gas" } else { "out of gas" }.to_string()
        }
        InstructionResult::OutOfFunds => if kind.is_parity() {
            "Insufficient balance for transfer"
        } else {
            "insufficient balance for transfer"
        }
        .to_string(),
        InstructionResult::MemoryOOG => {
            if kind.is_parity() { "Out of gas" } else { "out of gas: out of memory" }.to_string()
        }
        InstructionResult::MemoryLimitOOG => {
            if kind.is_parity() { "Out of gas" } else { "out of gas: reach memory limit" }
                .to_string()
        }
        InstructionResult::InvalidOperandOOG => {
            if kind.is_parity() { "Out of gas" } else { "out of gas: invalid operand" }.to_string()
        }
        InstructionResult::OpcodeNotFound => {
            if kind.is_parity() { "Bad instruction" } else { "invalid opcode" }.to_string()
        }
        InstructionResult::StackOverflow => "Out of stack".to_string(),
        InstructionResult::InvalidJump => {
            if kind.is_parity() { "Bad jump destination" } else { "invalid jump destination" }
                .to_string()
        }
        InstructionResult::PrecompileError => {
            if kind.is_parity() { "Built-in failed" } else { "precompiled failed" }.to_string()
        }
        InstructionResult::InvalidFEOpcode => {
            if kind.is_parity() { "Bad instruction" } else { "invalid opcode: INVALID" }.to_string()
        }
        InstructionResult::ReentrancySentryOOG => if kind.is_parity() {
            "Out of gas"
        } else {
            "out of gas: not enough gas for reentrancy sentry"
        }
        .to_string(),
        status => format!("{status:?}"),
    };

    Some(msg)
}

/// Formats memory data into a list of 32-byte hex-encoded chunks.
///
/// See: <https://github.com/ethereum/go-ethereum/blob/366d2169fbc0e0f803b68c042b77b6b480836dbc/eth/tracers/logger/logger.go#L450-L452>
pub(crate) fn convert_memory(data: &[u8]) -> Vec<String> {
    let mut memory = Vec::with_capacity(data.len().div_ceil(32));
    let (chunks, remainder) = data.as_chunks::<32>();
    for chunk in chunks {
        memory.push(hex::encode_prefixed(chunk));
    }
    if !remainder.is_empty() {
        let mut last_chunk = [0u8; 32];
        last_chunk[..remainder.len()].copy_from_slice(remainder);
        memory.push(hex::encode_prefixed(last_chunk));
    }
    memory
}

/// Get the gas used, accounting for refunds
#[inline]
pub(crate) fn gas_used(spec: SpecId, spent: u64, refunded: u64) -> u64 {
    let refund_quotient = if SpecId::is_enabled_in(spec, SpecId::LONDON) { 5 } else { 2 };
    spent - (refunded).min(spent / refund_quotient)
}

/// Loads the code for the given account from the account itself or the database
///
/// Returns None if the code hash is the KECCAK_EMPTY hash
#[inline]
pub(crate) fn load_account_code<DB: DatabaseRef>(
    db: DB,
    db_acc: &revm::state::AccountInfo,
) -> Option<Bytes> {
    db_acc.code.as_ref().map(|code| code.original_bytes()).or_else(|| {
        if db_acc.code_hash == KECCAK_EMPTY {
            None
        } else {
            db.code_by_hash_ref(db_acc.code_hash).ok().map(|code| code.original_bytes())
        }
    })
}

/// Returns a non-empty revert reason if the output is a revert/error.
/// Follows Geth's UnpackRevert logic
/// <https://github.com/ethereum/go-ethereum/blob/4414e2833f92f437d0a68b53ed95ac5756a90a16/accounts/abi/abi.go#L278>.
#[inline]
pub(crate) fn maybe_revert_reason(output: &[u8]) -> Option<String> {
    match GenericRevertReason::decode(output)? {
        GenericRevertReason::ContractError(err) => {
            let reason = match err {
                // return the raw revert reason and don't use the revert's display message
                ContractError::Revert(revert) => revert.reason,
                // return as geth panic error string
                ContractError::Panic(panic) => panic.as_geth_str().to_string(),
                err => err.to_string(),
            };

            if reason.is_empty() || reason.trim_matches('\0').is_empty() {
                None
            } else {
                Some(reason)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloy_sol_types::{GenericContractError, SolInterface};

    #[test]
    fn decode_revert_reason() {
        let err = GenericContractError::Revert("my revert".into());
        let encoded = err.abi_encode();
        let reason = maybe_revert_reason(&encoded).unwrap();
        assert_eq!(reason, "my revert");
    }

    // <https://etherscan.io/tx/0x105707c8e3b3675a8424a7b0820b271cbe394eaf4d5065b03c273298e3a81314>
    #[test]
    fn decode_revert_reason_with_error() {
        let err = hex!("08c379a000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000024556e697377617056323a20494e53554646494349454e545f494e5055545f414d4f554e5400000000000000000000000000000000000000000000000000000080");
        let reason = maybe_revert_reason(&err[..]).unwrap();
        assert_eq!(reason, "UniswapV2: INSUFFICIENT_INPUT_AMOUNT");
    }

    #[test]
    fn decode_revert_reason_with_null_bytes() {
        let empty_err = GenericContractError::Revert("".into());
        let encoded = empty_err.abi_encode();
        assert!(maybe_revert_reason(&encoded).is_none());

        let null_bytes_err =
            GenericContractError::Revert(String::from_utf8(vec![0u8; 32]).unwrap().into());
        let encoded = null_bytes_err.abi_encode();
        assert!(maybe_revert_reason(&encoded).is_none());
    }

    #[test]
    fn decode_revert_reason_with_invalid_selector() {
        // Test data that doesn't start with revert or panic selector
        let invalid_data = vec![0x12, 0x34, 0x56, 0x78]; // Invalid selector
        assert_eq!(
            maybe_revert_reason(&invalid_data),
            None,
            "Should return None for invalid selector"
        );

        // Test data too short
        let short_data = vec![0x08, 0xc3]; // Only 2 bytes
        assert_eq!(maybe_revert_reason(&short_data), None, "Should return None for data too short");

        // Test valid string but with proper selector validation
        let control_char_err = GenericContractError::Revert("\u{000f}.[l".into());
        let encoded = control_char_err.abi_encode();
        // This should now return the string since we're using Geth's validation logic
        assert_eq!(maybe_revert_reason(&encoded), Some("\u{000f}.[l".to_string()));

        // Test normal readable string
        let readable_err = GenericContractError::Revert("Normal error message".into());
        let encoded = readable_err.abi_encode();
        assert_eq!(maybe_revert_reason(&encoded), Some("Normal error message".to_string()));
    }

    #[test]
    fn decode_revert_reason_with_raw_string() {
        let non_readable_data = "\u{9a62}\u{0002}".as_bytes();
        assert_eq!(
            maybe_revert_reason(non_readable_data),
            None,
            "Should return None for raw strings"
        );
    }

    // <https://github.com/paradigmxyz/revm-inspectors/pull/353#issuecomment-36146608124>
    #[test]
    fn decode_string_revert() {
        let err = hex!("0x08c379a0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000214661696c656420746f2076657269667920424950333232207369676e617475726500000000000000000000000000000000000000000000000000000000000000");
        let reason = maybe_revert_reason(&err[..]).unwrap();
        assert_eq!(reason, "Failed to verify BIP322 signature".to_string());
    }

    #[test]
    fn convert_memory_prefixes_and_zero_pads_chunks() {
        let data = (0u8..33).collect::<Vec<_>>();

        assert_eq!(
            convert_memory(&data),
            vec![
                "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_string(),
                "0x2000000000000000000000000000000000000000000000000000000000000000".to_string(),
            ]
        );
    }
}
