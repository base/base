//! Conversions between the engine-neutral precompile types and `revm`.

use alloc::string::ToString;

use revm::{
    context::{ErasedError, journaled_state::JournalLoadError},
    precompile::{
        PrecompileError as RevmPrecompileError, PrecompileHalt as RevmPrecompileHalt,
        PrecompileOutput as RevmPrecompileOutput, PrecompileResult as RevmPrecompileResult,
        PrecompileStatus as RevmPrecompileStatus,
    },
    state::{AccountInfo as RevmAccountInfo, Bytecode as RevmBytecode},
};

use crate::{
    error::BasePrecompileError,
    neutral::{
        AccountInfo, Bytecode, PrecompileError, PrecompileHalt, PrecompileOutput, PrecompileResult,
        PrecompileStatus,
    },
};

impl From<&RevmAccountInfo> for AccountInfo {
    fn from(value: &RevmAccountInfo) -> Self {
        Self { balance: value.balance, nonce: value.nonce, code_hash: value.code_hash }
    }
}

impl From<&RevmBytecode> for Bytecode {
    fn from(value: &RevmBytecode) -> Self {
        value.eip7702_address().map_or_else(|| Self::Legacy(value.original_bytes()), Self::Eip7702)
    }
}

impl From<RevmBytecode> for Bytecode {
    fn from(value: RevmBytecode) -> Self {
        Self::from(&value)
    }
}

impl From<Bytecode> for RevmBytecode {
    fn from(value: Bytecode) -> Self {
        match value {
            Bytecode::Legacy(raw) => Self::new_legacy(raw),
            Bytecode::Eip7702(delegate) => Self::new_eip7702(delegate),
        }
    }
}

impl From<PrecompileHalt> for RevmPrecompileHalt {
    fn from(value: PrecompileHalt) -> Self {
        match value {
            PrecompileHalt::OutOfGas => Self::OutOfGas,
        }
    }
}

impl From<PrecompileStatus> for RevmPrecompileStatus {
    fn from(value: PrecompileStatus) -> Self {
        match value {
            PrecompileStatus::Success => Self::Success,
            PrecompileStatus::Revert => Self::Revert,
            PrecompileStatus::Halt(reason) => Self::Halt(reason.into()),
        }
    }
}

impl From<PrecompileError> for RevmPrecompileError {
    fn from(value: PrecompileError) -> Self {
        match value {
            PrecompileError::Fatal(msg) => Self::Fatal(msg),
        }
    }
}

impl From<PrecompileOutput> for RevmPrecompileOutput {
    fn from(value: PrecompileOutput) -> Self {
        Self {
            status: value.status.into(),
            gas_used: value.gas_used,
            gas_refunded: value.gas_refunded,
            state_gas_used: value.state_gas_used,
            state_gas_spilled: value.state_gas_spilled,
            reservoir: value.reservoir,
            bytes: value.bytes,
        }
    }
}

impl From<JournalLoadError<ErasedError>> for BasePrecompileError {
    fn from(value: JournalLoadError<ErasedError>) -> Self {
        match value {
            JournalLoadError::DBError(e) => Self::Fatal(e.to_string()),
            JournalLoadError::ColdLoadSkipped => Self::OutOfGas,
        }
    }
}

/// Converts a base [`PrecompileResult`] into the `revm` precompile result.
///
/// Applied at the precompile-dispatch boundary where a native precompile hands its result back to
/// revm.
pub trait IntoRevmPrecompileResult {
    /// Converts a base precompile result into the `revm` precompile result.
    fn into_revm(self) -> RevmPrecompileResult;
}

impl IntoRevmPrecompileResult for PrecompileResult {
    fn into_revm(self) -> RevmPrecompileResult {
        self.map(Into::into).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, KECCAK256_EMPTY, U256, address};

    use super::*;

    #[test]
    fn precompile_output_conversion_is_field_exact() {
        let base = PrecompileOutput {
            status: PrecompileStatus::Success,
            gas_used: 500,
            gas_refunded: 200,
            state_gas_used: 30,
            state_gas_spilled: 20,
            reservoir: 10,
            bytes: Bytes::from_static(&[1, 2, 3]),
        };
        let revm: RevmPrecompileOutput = base.clone().into();
        assert_eq!(revm.gas_used, 500);
        assert_eq!(revm.gas_refunded, 200);
        assert_eq!(revm.state_gas_used, 30);
        assert_eq!(revm.state_gas_spilled, 20);
        assert_eq!(revm.reservoir, 10);
        assert_eq!(revm.bytes, base.bytes);
        assert!(revm.is_success());
    }

    #[test]
    fn revert_and_halt_map_to_matching_status() {
        let revert: RevmPrecompileOutput = PrecompileOutput::revert(7, Bytes::new(), 3).into();
        assert!(revert.is_revert());
        assert_eq!(revert.gas_used, 7);

        let halt: RevmPrecompileOutput = PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0).into();
        assert!(halt.is_halt());
    }

    #[test]
    fn account_info_reads_match_revm() {
        let revm = RevmAccountInfo {
            balance: U256::from(42u64),
            nonce: 7,
            code_hash: KECCAK256_EMPTY,
            code: None,
            ..RevmAccountInfo::default()
        };
        let base = AccountInfo::from(&revm);
        assert_eq!(base.balance, U256::from(42u64));
        assert_eq!(base.nonce, 7);
        assert!(base.is_empty_code_hash());
    }

    #[test]
    fn bytecode_round_trip_preserves_code_and_hash() {
        let delegate = address!("0x00000000000000000000000000000000000000dd");
        for revm in [
            RevmBytecode::default(),
            RevmBytecode::new_legacy(Bytes::from_static(&[0xef])),
            RevmBytecode::new_raw(Bytes::from_static(&[0x60, 0x00])),
            RevmBytecode::new_eip7702(delegate),
        ] {
            let base = Bytecode::from(&revm);
            assert_eq!(base.original_bytes(), revm.original_bytes());
            assert_eq!(base.hash_slow(), revm.hash_slow());
            assert_eq!(base.len(), revm.len());

            let back = RevmBytecode::from(base);
            assert_eq!(back.original_bytes(), revm.original_bytes());
            assert_eq!(back.kind(), revm.kind());
        }
    }
}
