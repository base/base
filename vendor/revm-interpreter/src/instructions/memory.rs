use core::cmp::max;

use base_evm_context::Host;
use revm_primitives::U256;

use crate::{
    InstructionContext as Ictx, InstructionExecResult as Result,
    interpreter::resize_memory,
    interpreter_types::{MemoryTr, RuntimeFlag},
};

/// Implements the MLOAD instruction.
///
/// Loads a 32-byte word from memory.
pub fn mload<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    popn_top!([], top, context.interpreter);
    let offset = as_usize_or_fail!(context.interpreter, top);
    resize_memory(
        &mut context.interpreter.gas,
        &mut context.interpreter.memory,
        context.host.gas_params(),
        offset,
        32,
    )?;
    *top =
        U256::try_from_be_slice(context.interpreter.memory.slice_len(offset, 32).as_ref()).unwrap();
    Ok(())
}

/// Implements the MSTORE instruction.
///
/// Stores a 32-byte word to memory.
pub fn mstore<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    popn!([offset, value], context.interpreter);
    let offset = as_usize_or_fail!(context.interpreter, offset);
    context.interpreter.resize_memory(context.host.gas_params(), offset, 32)?;
    context.interpreter.memory.set(offset, &value.to_be_bytes::<32>());
    Ok(())
}

/// Implements the MSTORE8 instruction.
///
/// Stores a single byte to memory.
pub fn mstore8<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    popn!([offset, value], context.interpreter);
    let offset = as_usize_or_fail!(context.interpreter, offset);
    context.interpreter.resize_memory(context.host.gas_params(), offset, 1)?;
    context.interpreter.memory.set(offset, &[value.byte(0)]);
    Ok(())
}

/// Implements the MSIZE instruction.
///
/// Gets the size of active memory in bytes.
pub fn msize<H: ?Sized>(context: Ictx<'_, H>) -> Result {
    push!(context.interpreter, U256::from(context.interpreter.memory.size()));
    Ok(())
}

/// Implements the MCOPY instruction.
///
/// EIP-5656: Memory copying instruction that copies memory from one location to another.
pub fn mcopy<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    check!(context.interpreter, CANCUN);
    popn!([dst, src, len], context.interpreter);

    // Into usize or fail
    let len = as_usize_or_fail!(context.interpreter, len);
    // Deduce gas
    gas!(context.interpreter, context.host.gas_params().mcopy_cost(len));

    if len == 0 {
        return Ok(());
    }

    let dst = as_usize_or_fail!(context.interpreter, dst);
    let src = as_usize_or_fail!(context.interpreter, src);
    // Resize memory
    context.interpreter.resize_memory(context.host.gas_params(), max(dst, src), len)?;
    // Copy memory in place
    context.interpreter.memory.copy(dst, src, len);
    Ok(())
}
