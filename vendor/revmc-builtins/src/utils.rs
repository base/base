use core::{hint::cold_path, num::NonZero};

use revm_context_interface::journaled_state::AccountInfoLoad;
use revm_interpreter::{InstructionResult, as_usize_saturated, host::LoadError};
use revm_primitives::Address;
use revmc_context::{EvmContext, EvmWord};

pub type BuiltinResult = Result<(), BuiltinError>;

/// Represents an error that occurred during a builtin execution.
#[derive(Debug)]
#[repr(transparent)]
pub struct BuiltinError(NonZero<u8>);

impl From<BuiltinError> for InstructionResult {
    #[inline]
    fn from(value: BuiltinError) -> Self {
        // SAFETY: BuiltinError is always created from a valid InstructionResult.
        unsafe { core::mem::transmute::<_, _>(value.0.get()) }
    }
}

impl From<InstructionResult> for BuiltinError {
    #[inline]
    fn from(value: InstructionResult) -> Self {
        cold_path();
        Self(unsafe { NonZero::new_unchecked(value as u8) })
    }
}

impl From<LoadError> for BuiltinError {
    #[inline]
    fn from(value: LoadError) -> Self {
        cold_path();
        match value {
            LoadError::ColdLoadSkipped => InstructionResult::OutOfGas.into(),
            LoadError::DBError => InstructionResult::FatalExternalError.into(),
        }
    }
}

/// Extension trait to convert `Option<T>` to `BuiltinResult`.
pub(crate) trait OkOrFatal<T> {
    fn ok_or_fatal(self) -> Result<T, BuiltinError>;
}

impl<T> OkOrFatal<T> for Option<T> {
    #[inline]
    fn ok_or_fatal(self) -> Result<T, BuiltinError> {
        self.ok_or_else(|| InstructionResult::FatalExternalError.into())
    }
}

/// Loads an account, handling cold load gas accounting.
///
/// Pre-Berlin, `cold_account_additional_cost` is 0, so the cold load logic is a no-op.
pub(crate) fn load_account<'a>(
    ecx: &'a mut EvmContext<'_>,
    address: Address,
    load_code: bool,
) -> Result<AccountInfoLoad<'a>, BuiltinError> {
    let cold_load_gas = ecx.gas_params.cold_account_additional_cost();
    let skip_cold_load = ecx.gas.remaining() < cold_load_gas;
    let account = ecx.host.load_account_info_skip_cold_load(address, load_code, skip_cold_load)?;
    if account.is_cold {
        gas!(ecx, cold_load_gas);
    }
    Ok(account)
}

/// Splits the stack pointer into `N` elements by casting it to an array.
///
/// NOTE: this returns the arguments in **reverse order**. Use [`read_words!`] to get them in order.
///
/// The returned lifetime is valid for the entire duration of the builtin.
///
/// # Safety
///
/// Caller must ensure that `N` matches the number of elements popped in JIT code.
#[inline(always)]
pub(crate) unsafe fn read_words_rev<'a, const N: usize>(sp: *mut EvmWord) -> &'a mut [EvmWord; N] {
    unsafe { &mut *sp.cast::<[EvmWord; N]>() }
}

#[inline]
pub(crate) fn ensure_memory(ecx: &mut EvmContext<'_>, offset: usize, len: usize) -> BuiltinResult {
    revm_interpreter::interpreter::resize_memory(
        &mut ecx.gas,
        ecx.memory,
        &ecx.gas_params,
        offset,
        len,
    )?;
    ecx.refresh_memory_cache();
    Ok(())
}

pub(crate) unsafe fn copy_operation(
    ecx: &mut EvmContext<'_>,
    rev![memory_offset, data_offset, len]: &mut [EvmWord; 3],
    data: &[u8],
) -> BuiltinResult {
    let len = try_into_usize!(len);
    if len != 0 {
        gas!(ecx, ecx.gas_params.copy_cost(len));
        let memory_offset = try_into_usize!(memory_offset);
        ensure_memory(ecx, memory_offset, len)?;
        let data_offset = data_offset.to_u256();
        let data_offset = as_usize_saturated!(data_offset);
        ecx.memory.set_data(memory_offset, data_offset, len, data);
    }
    Ok(())
}
