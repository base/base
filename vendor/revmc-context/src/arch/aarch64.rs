use core::ptr::NonNull;

use revm_interpreter::InstructionResult;

use super::{EXIT_RESULT_OFFSET, EXIT_SP_OFFSET};
use crate::{EvmContext, EvmStack, RawEvmCompilerFn};

/// Entry trampoline: saves callee-saved registers, stores SP into
/// `ecx.exit_sp`, and calls the JIT function.
///
/// On normal return the JIT function's `InstructionResult` is forwarded.
/// On abnormal exit (builtin error), [`revmc_exit`] restores SP and
/// returns the `exit_result` stored in `EvmContext`.
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn revmc_entry(
    ecx: NonNull<EvmContext<'_>>,
    stack: NonNull<EvmStack>,
    stack_len: NonNull<usize>,
    f: RawEvmCompilerFn,
) -> InstructionResult {
    // AAPCS64: x0=ecx, x1=stack, x2=stack_len, x3=f
    // The JIT function takes (ecx, stack, stack_len) — already in x0, x1, x2.
    //
    // Frame layout (160 bytes, 16-byte aligned):
    //   [sp, #0]   x29/x30 (FP/LR)
    //   [sp, #16]  x19/x20
    //   [sp, #32]  x21/x22
    //   [sp, #48]  x23/x24
    //   [sp, #64]  x25/x26
    //   [sp, #80]  x27/x28
    //   [sp, #96]  d8/d9
    //   [sp, #112] d10/d11
    //   [sp, #128] d12/d13
    //   [sp, #144] d14/d15
    core::arch::naked_asm!(
        // Save FP/LR, callee-saved GPRs (x19-x28), and NEON regs (d8-d15).
        "stp x29, x30, [sp, #-160]!",
        "stp x19, x20, [sp, #16]",
        "stp x21, x22, [sp, #32]",
        "stp x23, x24, [sp, #48]",
        "stp x25, x26, [sp, #64]",
        "stp x27, x28, [sp, #80]",
        "stp d8,  d9,  [sp, #96]",
        "stp d10, d11, [sp, #112]",
        "stp d12, d13, [sp, #128]",
        "stp d14, d15, [sp, #144]",
        // Save SP into ecx->exit_sp.
        "mov x9, sp",
        "str x9, [x0, {exit_sp}]",
        "blr x3",
        // Normal return — w0 = InstructionResult.
        "ldp d14, d15, [sp, #144]",
        "ldp d12, d13, [sp, #128]",
        "ldp d10, d11, [sp, #112]",
        "ldp d8,  d9,  [sp, #96]",
        "ldp x27, x28, [sp, #80]",
        "ldp x25, x26, [sp, #64]",
        "ldp x23, x24, [sp, #48]",
        "ldp x21, x22, [sp, #32]",
        "ldp x19, x20, [sp, #16]",
        "ldp x29, x30, [sp], #160",
        "ret",
        exit_sp = const EXIT_SP_OFFSET,
    )
}

/// Exit trampoline: loads `ecx.exit_result`, restores the saved SP,
/// pops callee-saved registers and returns to the caller of `revmc_entry`.
///
/// # Safety
///
/// Must only be called from a builtin that was invoked through `revmc_entry`.
#[unsafe(naked)]
pub unsafe extern "C" fn revmc_exit(ecx: *const EvmContext<'_>) -> ! {
    core::arch::naked_asm!(
        // Restore the saved SP (must read before clobbering x0).
        "ldr x9, [x0, {exit_sp}]",
        // Load exit_result into the return register.
        "ldrb w0, [x0, {exit_result}]",
        "mov sp, x9",
        // Restore callee-saved registers and return.
        "ldp d14, d15, [sp, #144]",
        "ldp d12, d13, [sp, #128]",
        "ldp d10, d11, [sp, #112]",
        "ldp d8,  d9,  [sp, #96]",
        "ldp x27, x28, [sp, #80]",
        "ldp x25, x26, [sp, #64]",
        "ldp x23, x24, [sp, #48]",
        "ldp x21, x22, [sp, #32]",
        "ldp x19, x20, [sp, #16]",
        "ldp x29, x30, [sp], #160",
        "ret",
        exit_result = const EXIT_RESULT_OFFSET,
        exit_sp = const EXIT_SP_OFFSET,
    )
}
