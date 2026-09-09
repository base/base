use revm_primitives::hardfork::SpecId::*;

use crate::{Host, InstructionContext as Ictx, InstructionExecResult as Result};

/// EIP-1344: ChainID opcode
pub fn chainid<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    check!(context.interpreter, ISTANBUL);
    push!(context.interpreter, context.host.chain_id());
    Ok(())
}

/// Implements the COINBASE instruction.
///
/// Pushes the current block's beneficiary address onto the stack.
pub fn coinbase<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    push!(context.interpreter, context.host.beneficiary().into_word().into());
    Ok(())
}

/// Implements the TIMESTAMP instruction.
///
/// Pushes the current block's timestamp onto the stack.
pub fn timestamp<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    push!(context.interpreter, context.host.timestamp());
    Ok(())
}

/// Implements the NUMBER instruction.
///
/// Pushes the current block number onto the stack.
pub fn block_number<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    push!(context.interpreter, context.host.block_number());
    Ok(())
}

/// Implements the DIFFICULTY/PREVRANDAO instruction.
///
/// Pushes the block difficulty (pre-merge) or prevrandao (post-merge) onto the stack.
pub fn difficulty<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    if context.interpreter.runtime_flag.spec_id.is_enabled_in(MERGE) {
        // Unwrap is safe as this fields is checked in validation handler.
        push!(context.interpreter, context.host.prevrandao().unwrap());
    } else {
        push!(context.interpreter, context.host.difficulty());
    }
    Ok(())
}

/// Implements the GASLIMIT instruction.
///
/// Pushes the current block's gas limit onto the stack.
pub fn gaslimit<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    push!(context.interpreter, context.host.gas_limit());
    Ok(())
}

/// EIP-3198: BASEFEE opcode
pub fn basefee<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    check!(context.interpreter, LONDON);
    push!(context.interpreter, context.host.basefee());
    Ok(())
}

/// EIP-7516: BLOBBASEFEE opcode
pub fn blob_basefee<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    check!(context.interpreter, CANCUN);
    push!(context.interpreter, context.host.blob_gasprice());
    Ok(())
}

/// EIP-7843: SLOTNUM opcode
pub fn slot_num<H: Host + ?Sized>(context: Ictx<'_, H>) -> Result {
    check!(context.interpreter, AMSTERDAM);
    push!(context.interpreter, context.host.slot_num());
    Ok(())
}
