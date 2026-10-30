//! Conversions between the engine-neutral precompile types and evm2.

use alloy_primitives::Address;
use evm2::{
    PrecompileError as Evm2PrecompileError, PrecompileHalt as Evm2PrecompileHalt,
    bytecode::Bytecode as Evm2Bytecode, interpreter::GasTracker,
    precompile::PrecompileOutput as Evm2PrecompileOutput,
    precompiles::PrecompileResult as Evm2PrecompileResult,
};

use crate::neutral::{
    Bytecode, PrecompileError, PrecompileHalt, PrecompileResult, PrecompileStatus,
};

impl From<&Evm2Bytecode> for Bytecode {
    fn from(value: &Evm2Bytecode) -> Self {
        value.eip7702_address().map_or_else(|| Self::Legacy(value.original_bytes()), Self::Eip7702)
    }
}

impl From<Bytecode> for Evm2Bytecode {
    fn from(value: Bytecode) -> Self {
        match value {
            Bytecode::Legacy(raw) => Self::new_legacy(raw),
            Bytecode::Eip7702(delegate) => Self::new_eip7702(delegate),
        }
    }
}

/// Supplies the executing transaction's origin (`tx.origin`) to native precompiles.
///
/// evm2 precompile dispatch receives only the frame message, not the transaction environment, so
/// the EVM's instance extension (`EvmTypesHost::EvmExt`) carries the origin for the transaction
/// currently executing.
pub trait Evm2TxOrigin {
    /// Returns the origin of the transaction currently executing.
    fn tx_origin(&self) -> Address;
}

/// Converts a base [`PrecompileResult`] into the evm2 precompile result and frame gas.
///
/// Rebuilds `gas` from the output exactly as revm's precompile frame does: `gas_used` is spent from
/// the call gas limit, the refund and state-gas counters are copied, and the reservoir becomes
/// [`PrecompileOutput::reservoir`](crate::PrecompileOutput::reservoir). Spending more than the
/// limit, or halting out of gas, consumes all call gas.
pub trait IntoEvm2PrecompileResult {
    /// Converts a base precompile result into the evm2 precompile result, updating `gas`.
    fn into_evm2(self, gas: &mut GasTracker) -> Evm2PrecompileResult;
}

impl IntoEvm2PrecompileResult for PrecompileResult {
    fn into_evm2(self, gas: &mut GasTracker) -> Evm2PrecompileResult {
        let output =
            self.map_err(|PrecompileError::Fatal(message)| Evm2PrecompileError::from(message))?;

        let gas_limit = gas.limit();
        *gas = GasTracker::new_used_gas(gas_limit, output.gas_used, output.reservoir);
        gas.set_refunded(output.gas_refunded);
        gas.add_state_gas_spent(output.state_gas_used);
        gas.add_state_gas_spilled(output.state_gas_spilled);

        if output.gas_used > gas_limit {
            gas.spend_all();
            return Err(Evm2PrecompileHalt::OutOfGas.into());
        }
        match output.status {
            PrecompileStatus::Success => Ok(Evm2PrecompileOutput::new(output.bytes)),
            PrecompileStatus::Revert => Err(Evm2PrecompileError::Revert(output.bytes)),
            PrecompileStatus::Halt(PrecompileHalt::OutOfGas) => {
                gas.spend_all();
                Err(Evm2PrecompileHalt::OutOfGas.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};

    use super::*;
    use crate::PrecompileOutput;

    #[test]
    fn bytecode_round_trip_preserves_code_and_hash() {
        let delegate = address!("0x00000000000000000000000000000000000000dd");
        for evm2 in [
            Evm2Bytecode::default(),
            Evm2Bytecode::new_legacy(Bytes::from_static(&[0xef])),
            Evm2Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00])),
            Evm2Bytecode::new_eip7702(delegate),
        ] {
            let base = Bytecode::from(&evm2);
            assert_eq!(base.original_bytes(), evm2.original_bytes());
            assert_eq!(base.hash_slow(), evm2.hash_slow());

            let back = Evm2Bytecode::from(base);
            assert_eq!(back.original_bytes(), evm2.original_bytes());
            assert_eq!(back.kind(), evm2.kind());
        }
    }

    #[test]
    fn success_spends_gas_used_and_keeps_refund() {
        let mut gas = GasTracker::new(1_000);
        let mut output = PrecompileOutput::new(400, Bytes::from_static(b"ok"), 0);
        output.gas_refunded = 50;

        let result = Ok(output).into_evm2(&mut gas).unwrap();

        assert_eq!(result.bytes(), b"ok");
        assert_eq!(gas.spent(), 400);
        assert_eq!(gas.refunded(), 50);
    }

    #[test]
    fn revert_returns_output_and_unspent_gas() {
        let mut gas = GasTracker::new(1_000);

        let result =
            Ok(PrecompileOutput::revert(300, Bytes::from_static(b"no"), 0)).into_evm2(&mut gas);

        assert!(
            matches!(result, Err(Evm2PrecompileError::Revert(bytes)) if bytes.as_ref() == b"no")
        );
        assert_eq!(gas.remaining(), 700);
    }

    #[test]
    fn out_of_gas_halt_consumes_all_gas() {
        let mut gas = GasTracker::new(1_000);

        let result = Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0))
            .into_evm2(&mut gas)
            .unwrap_err();

        assert!(result.is_oog());
        assert_eq!(gas.remaining(), 0);
    }

    #[test]
    fn overspend_is_out_of_gas() {
        let mut gas = GasTracker::new(1_000);

        let result =
            Ok(PrecompileOutput::new(1_001, Bytes::new(), 0)).into_evm2(&mut gas).unwrap_err();

        assert!(result.is_oog());
        assert_eq!(gas.remaining(), 0);
    }

    #[test]
    fn fatal_error_aborts() {
        let mut gas = GasTracker::new(1_000);

        let result = Err(PrecompileError::Fatal("db".into())).into_evm2(&mut gas).unwrap_err();

        assert!(result.is_fatal());
    }
}
