//! Intra-block dead store elimination.
//!
//! Walks each basic block backward to determine which stack positions are live (will be read
//! by a later instruction or at the block exit). Instructions whose outputs are all dead and
//! whose logic is safe to eliminate are marked [`InstFlags::NOOP`].
//!
//! DUP, SWAP, POP, DUPN, SWAPN, and EXCHANGE require custom liveness transfer functions
//! because generic `(inputs, outputs)` arity doesn't capture how they map stack slots:
//! - SWAP/SWAPN permute two positions without consuming or producing values.
//! - DUP/DUPN copy a single deep source to a new TOS.
//! - EXCHANGE swaps two arbitrary non-TOS positions.
//! - POP kills its input (makes the position dead) rather than reading it.

use bitvec::vec::BitVec;
use revm_bytecode::opcode as op;

use super::{StackSectionAnalysis, block_analysis::AbsValue};
use crate::bytecode::{AnalysisConfig, Bytecode, InstFlags};

/// Decoded immediate for liveness transfer of DUPN/SWAPN/EXCHANGE.
#[derive(Clone, Copy)]
enum DecodedImm {
    /// Single depth (DUPN, SWAPN).
    Single(u8),
    /// Pair of non-TOS indices (EXCHANGE).
    Pair(u8, u8),
}

impl Bytecode<'_> {
    /// Intra-block dead store elimination.
    ///
    /// Walks each block backward to determine which stack positions are live (will be read
    /// by a later instruction or at the block exit). Instructions that are safe to skip and
    /// whose outputs are all dead are marked `NOOP`.
    #[instrument(name = "dse", level = "debug", skip_all)]
    pub(crate) fn dead_store_elim(&mut self) {
        let mut eliminated = 0u32;
        let inspect_stack = self.config.contains(AnalysisConfig::INSPECT_STACK);

        // Reusable buffers hoisted out of the per-block loop.
        let mut heights = Vec::new();
        let mut live = BitVec::new();

        for bid in self.cfg.blocks.indices() {
            let block = &self.cfg.blocks[bid];
            // Compute the stack height at each instruction boundary by walking forward.
            // heights[i] = stack height *before* executing insts[i].
            // heights[insts.len()] = stack height after the last instruction.
            let mut stack = StackSectionAnalysis::default();
            heights.clear();
            heights.push(0); // placeholder; patched to entry_height below.
            for inst in block.insts() {
                let (inp, out) = self.insts[inst].stack_io();
                stack.process(inp, out);
                heights.push(stack.diff());
            }

            // Shift all heights so that heights[0] == entry_height.
            let section = stack.section();
            let entry_height = section.inputs as i32;
            for h in &mut heights {
                *h += entry_height;
            }

            let exit_height = *heights.last().unwrap();
            let max_height = entry_height + section.max_growth as i32;

            // If the block's terminator diverges, no successor will read the stack,
            // so all exit positions are dead. Unless `inspect_stack` is set (the caller
            // observes every store) or the block contains a suspending instruction
            // (the function can be re-entered and the caller reads the stack).
            let has_suspend = block.insts().any(|i| self.insts[i].may_suspend());
            let term_diverges =
                !inspect_stack && !has_suspend && self.insts[block.terminator()].is_diverging();

            live.clear();
            live.resize(exit_height.max(0) as usize, !term_diverges);
            live.resize(max_height.max(0) as usize, false);

            // Nothing to do.
            if live.is_empty() {
                continue;
            }

            // Walk backward.
            for (idx, inst) in block.insts().enumerate().rev() {
                let data = &self.insts[inst];
                if data.flags.contains(InstFlags::NOOP)
                    || data.flags.contains(InstFlags::DISABLED)
                    || data.flags.contains(InstFlags::UNKNOWN)
                {
                    continue;
                }

                let opcode = data.opcode;
                let (inp, out) = data.stack_io();
                let h_before = heights[idx] as usize;

                // Decode immediate for DUPN/SWAPN/EXCHANGE.
                let imm = match opcode {
                    op::DUPN | op::SWAPN => {
                        crate::decode_single(data.imm_byte()).map(DecodedImm::Single)
                    }
                    op::EXCHANGE => {
                        crate::decode_pair(data.imm_byte()).map(|(n, m)| DecodedImm::Pair(n, m))
                    }
                    _ => None,
                };

                // Check if all outputs are dead and the instruction can be turned into a no-op.
                if can_skip_when_dead(opcode)
                    && !data.is_branching()
                    && !data.is_diverging()
                    && all_outputs_dead(&live, opcode, h_before, inp as usize, out as usize, imm)
                {
                    self.insts[inst].flags |= InstFlags::NOOP;
                    eliminated += 1;
                    continue;
                }

                transfer_liveness(
                    &mut live,
                    opcode,
                    h_before,
                    inp as usize,
                    out as usize,
                    imm,
                    &self.snapshots.inputs[inst],
                    self.snapshots.outputs.get(inst).copied().flatten(),
                );

                // Suspending instructions (CALL/CREATE*) trigger `copy_stack_to_arg()`
                // which copies the entire live stack prefix back to the interpreter.
                // All positions in that prefix must be physically materialized — we
                // cannot rely on later rematerialization since the suspend path runs
                // before any consumer.
                if data.may_suspend() {
                    let h_after = heights[idx + 1] as usize;
                    live[..h_after].fill(true);
                }
            }
        }

        if eliminated > 0 {
            debug!(eliminated, "dead stores eliminated");
        }
    }
}

/// Returns `true` if the opcode's logic is safe to skip when its outputs are dead.
///
/// This covers opcodes with no side effects beyond stack I/O and no dynamic gas. When
/// NOOP is set the instruction is still present (static gas is charged, stack
/// length is adjusted), only the computation is elided.
///
/// Based on solc's `SemanticInformation::movable()` with additions for call-constant
/// environment reads that are pure in EVM semantics. Excludes opcodes with dynamic gas
/// (EXP), memory access (MLOAD, KECCAK256), storage/host reads (SLOAD, TLOAD, BALANCE,
/// etc.), and MSIZE/GAS (position-dependent).
fn can_skip_when_dead(opcode: u8) -> bool {
    matches!(
        opcode,
        // Arithmetic (inline).
        op::ADD
            | op::MUL
            | op::SUB
            | op::SIGNEXTEND
            // Arithmetic (builtin-delegated).
            | op::DIV
            | op::SDIV
            | op::MOD
            | op::SMOD
            | op::ADDMOD
            | op::MULMOD
            // Comparison.
            | op::LT
            | op::GT
            | op::SLT
            | op::SGT
            | op::EQ
            | op::ISZERO
            // Bitwise.
            | op::AND
            | op::OR
            | op::XOR
            | op::NOT
            | op::BYTE
            | op::SHL
            | op::SHR
            | op::SAR
            | op::CLZ
            // Stack shuffles.
            | op::DUP1..=op::DUP16
            | op::DUPN
            | op::SWAP1..=op::SWAP16
            | op::SWAPN
            | op::EXCHANGE
            // Constants / push.
            | op::PUSH0..=op::PUSH32
            | op::PC
            | op::CODESIZE
            // Pure environment reads (no dynamic gas, no side effects).
            | op::ADDRESS
            | op::ORIGIN
            | op::CALLER
            | op::CALLVALUE
            | op::CALLDATALOAD
            | op::CALLDATASIZE
            | op::GASPRICE
            | op::RETURNDATASIZE
            | op::COINBASE
            | op::TIMESTAMP
            | op::NUMBER
            | op::DIFFICULTY
            | op::GASLIMIT
            | op::CHAINID
            | op::BASEFEE
            | op::BLOBBASEFEE
            | op::BLOBHASH
            | op::SLOTNUM
    )
}

/// Backward liveness transfer for a single instruction.
///
/// Updates `live` to reflect liveness *before* the instruction executes, given
/// `h_before` (the stack height before the instruction). All positions are guaranteed
/// in-bounds since `live` is sized to the block's max stack height.
///
/// `imm` carries the decoded immediate for DUPN/SWAPN/EXCHANGE; `None` for all other opcodes.
///
/// `input_snap` and `output` carry abstract interpretation snapshots. When the output is a
/// known constant, codegen replaces the entire instruction with a constant push and none of
/// the inputs need to be live. When individual inputs are known constants, codegen loads them
/// as immediates (`const_operand`) so those stack positions don't need to be live either.
#[allow(clippy::too_many_arguments)]
fn transfer_liveness(
    live: &mut BitVec,
    opcode: u8,
    h_before: usize,
    inp: usize,
    out: usize,
    imm: Option<DecodedImm>,
    input_snap: &[AbsValue],
    output: Option<AbsValue>,
) {
    match opcode {
        op::SWAP1..=op::SWAP16 => {
            let depth = (opcode - op::SWAP1 + 1) as usize;
            transfer_swap(live, h_before, depth);
        }
        op::SWAPN => {
            if let Some(DecodedImm::Single(depth)) = imm
                && (depth as usize) < h_before
            {
                transfer_swap(live, h_before, depth as usize);
            }
            // Infeasible depth or decode failure: conservative no-op.
        }
        op::DUP1..=op::DUP16 => {
            let depth = (opcode - op::DUP1 + 1) as usize;
            transfer_dup(live, h_before, depth);
        }
        op::DUPN => {
            if let Some(DecodedImm::Single(depth)) = imm
                && depth >= 1
                && (depth as usize) <= h_before
            {
                transfer_dup(live, h_before, depth as usize);
            }
        }
        op::EXCHANGE => {
            if let Some(DecodedImm::Pair(n, m)) = imm
                && (m as usize) < h_before
            {
                let tos = h_before - 1;
                let pos_a = tos - n as usize;
                let pos_b = tos - m as usize;
                let (a, b) = (live[pos_a], live[pos_b]);
                live.set(pos_a, b);
                live.set(pos_b, a);
            }
        }
        op::POP => {
            live.set(h_before - 1, false);
        }
        _ => generic_transfer(live, h_before, inp, out, input_snap, output),
    }
}

/// Generic liveness transfer: kill all outputs, then mark all inputs as live.
///
/// When the output is a known constant, codegen replaces the entire instruction with a
/// constant push and never reads any inputs from the stack, so none of them need to be live.
/// When individual inputs are known constants, codegen pre-writes them into the stack
/// slots (via `operand_value_or_load` or `sp_after_inputs`), so those positions don't
/// need to be live.
fn generic_transfer(
    live: &mut BitVec,
    h_before: usize,
    inp: usize,
    out: usize,
    input_snap: &[AbsValue],
    output: Option<AbsValue>,
) {
    let write_base = h_before - inp;
    // Kill outputs.
    for k in 0..out {
        live.set(write_base + k, false);
    }
    // If the output is a known constant, codegen replaces the entire instruction
    // with a constant push — none of the inputs are read from the stack.
    if out > 0 && matches!(output, Some(AbsValue::Const(_))) {
        return;
    }
    // Mark inputs live, skipping positions whose values are known constants.
    // Inline ops load constants via `operand_value_or_load`; builtin-delegated ops
    // have their constant operands pre-written into the stack slots by `sp_after_inputs`.
    // Both paths ensure the value is available at runtime, so const inputs can be
    // killed for any opcode.
    for k in 0..inp {
        // input_snap is in stack order: [deepest, ..., TOS].
        if input_snap.get(k).is_some_and(|v| matches!(v, AbsValue::Const(_))) {
            continue;
        }
        live.set(h_before - inp + k, true);
    }
}

/// SWAP liveness: permute liveness of TOS and the swapped position.
fn transfer_swap(live: &mut BitVec, h_before: usize, depth: usize) {
    let tos = h_before - 1;
    let other = tos - depth;
    let (a, b) = (live[tos], live[other]);
    live.set(tos, b);
    live.set(other, a);
}

/// Returns `true` if all positions actually written by the instruction are dead.
///
/// Stack shuffles need special handling because their `stack_io` arities include
/// pass-through slots (e.g. SWAP15 reports `stack_io(16,16)` but only writes 2
/// positions). All other opcodes use a generic check over their output range.
fn all_outputs_dead(
    live: &BitVec,
    opcode: u8,
    h_before: usize,
    inp: usize,
    out: usize,
    imm: Option<DecodedImm>,
) -> bool {
    match (opcode, imm) {
        // SWAP: only TOS and the deep slot are written.
        (op::SWAP1..=op::SWAP16, _) => {
            swap_all_dead(live, h_before, (opcode - op::SWAP1 + 1) as usize)
        }
        (op::SWAPN, Some(DecodedImm::Single(d))) if (d as usize) < h_before => {
            swap_all_dead(live, h_before, d as usize)
        }
        // DUP: only the new TOS is a real output.
        (op::DUP1..=op::DUP16, _) => !live[h_before],
        (op::DUPN, Some(DecodedImm::Single(_))) => !live[h_before],
        // EXCHANGE: only the two exchanged non-TOS positions are written.
        (op::EXCHANGE, Some(DecodedImm::Pair(n, m))) if (m as usize) < h_before => {
            let tos = h_before - 1;
            !live[tos - n as usize] && !live[tos - m as usize]
        }
        // Infeasible immediates — conservatively not dead.
        (op::DUPN | op::SWAPN | op::EXCHANGE, _) => false,
        // Generic: all output positions must be dead.
        _ if out > 0 => {
            let write_base = h_before - inp;
            (0..out).all(|k| {
                let pos = write_base + k;
                pos < live.len() && !live[pos]
            })
        }
        _ => false,
    }
}

/// SWAP dead check: both TOS and the swapped position must be dead.
fn swap_all_dead(live: &BitVec, h_before: usize, depth: usize) -> bool {
    let tos = h_before - 1;
    !live[tos] && !live[tos - depth]
}

/// DUP liveness: the new TOS is killed; if it was live, the source becomes live.
fn transfer_dup(live: &mut BitVec, h_before: usize, depth: usize) {
    let new_tos = h_before;
    let src = h_before - depth;
    let new_tos_live = live[new_tos];
    live.set(new_tos, false);
    if new_tos_live {
        live.set(src, true);
    }
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;
    use revm_primitives::hardfork::SpecId;

    use super::super::block_analysis::tests::{
        Inst, analyze_asm, analyze_asm_spec, analyze_code_spec,
    };
    use crate::bytecode::InstFlags;

    /// Helper: generates `"PUSH0\n"` repeated `n` times.
    fn push0s(n: usize) -> String {
        "PUSH0\n".repeat(n)
    }

    /// Helper: `n` PUSH0 padding followed by `suffix` asm, analyzed with AMSTERDAM spec.
    fn with_prefix(n: usize, suffix: &str) -> crate::bytecode::Bytecode<'static> {
        analyze_asm_spec(&format!("{}{suffix}", push0s(n)), SpecId::AMSTERDAM)
    }

    /// Helper: `PUSH0 CALLDATALOAD` (dynamic value) + `n` PUSH0 padding + `suffix` asm.
    fn with_dynamic_and_prefix(n: usize, suffix: &str) -> crate::bytecode::Bytecode<'static> {
        analyze_asm_spec(&format!("PUSH0 CALLDATALOAD {}{suffix}", push0s(n)), SpecId::AMSTERDAM)
    }

    #[test]
    fn push_pop() {
        let bytecode = analyze_asm(
            "
            PUSH1 0x42
            POP
            STOP
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH should be skipped (dead store)"
        );
    }

    #[test]
    fn push_push_add_pop() {
        let bytecode = analyze_asm(
            "
            PUSH1 0x03
            PUSH1 0x04
            ADD
            POP
            STOP
        ",
        );
        for (i, name) in [(0, "PUSH 3"), (1, "PUSH 4"), (2, "ADD")] {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "{name} should be skipped"
            );
        }
    }

    #[test]
    fn partial() {
        let bytecode = analyze_asm(
            "
            PUSH1 0x01
            PUSH1 0x02
            POP
            PUSH0
            MSTORE
            STOP
        ",
        );
        // PUSH 1 feeds MSTORE's value input; known constant, so codegen pre-writes it.
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH 1 should be skipped (const input)"
        );
        assert!(
            bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "PUSH 2 should be skipped"
        );
    }

    #[test]
    fn swap_preserves_liveness() {
        // SWAP1 swaps TOS and TOS-1. TOS is consumed by RET_WORD.
        // Uses CALLDATALOAD for dynamic values to prevent const-input DSE.
        let bytecode = analyze_asm(
            "
            PUSH0           ; inst 0
            CALLDATALOAD    ; inst 1: dynamic A
            PUSH1 0x20      ; inst 2
            CALLDATALOAD    ; inst 3: dynamic B
            SWAP1           ; inst 4: [B, A]
            POP             ; inst 5: [B]
            RET_WORD        ; inst 6..11: return B
        ",
        );
        // A is swapped to TOS then popped → dead. B is consumed by RET_WORD → live.
        assert!(
            bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD A should be skipped (dead after swap+pop)"
        );
        assert!(
            !bytecode.inst(Inst::from_usize(3)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD B should NOT be skipped (consumed by RET_WORD)"
        );
    }

    #[test]
    fn swap_dead_with_diverging_terminator() {
        // All values left on the stack at STOP are dead.
        let bytecode = analyze_asm(
            "
            PUSH1 0x01
            PUSH1 0x02
            PUSH1 0x03
            SWAP2
            STOP
        ",
        );
        for i in 0..4 {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "inst at {i} should be skipped (exit stack dead)"
            );
        }
    }

    #[test]
    fn dup_const_source_killed() {
        // DUP copy is popped (dead), PUSH 0x42 feeds MSTORE which pre-writes
        // constant operands, so the PUSH is dead.
        let bytecode = analyze_asm(
            "
            PUSH1 0x42
            DUP1
            POP
            PUSH0
            MSTORE
            STOP
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH should be skipped (const input)"
        );
    }

    #[test]
    fn dup_both_dead() {
        // DUP copies source to new TOS; if both the copy and the source are popped,
        // the producer should be eliminable.
        let bytecode = analyze_asm(
            "
            PUSH1 0x42
            DUP1
            POP
            POP
            STOP
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH should be skipped when both DUP copy and source are dead"
        );
    }

    #[test]
    fn env_read_dead() {
        // Call-constant environment reads should be eliminable when dead.
        let bytecode = analyze_asm(
            "
            ADDRESS
            POP
            STOP
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "ADDRESS should be skipped when dead"
        );
    }

    #[test]
    fn no_side_effects() {
        let bytecode = analyze_asm(
            "
            PUSH1 0x42
            PUSH0
            MSTORE
            STOP
        ",
        );
        assert!(!bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP));
    }

    #[test]
    fn swapn_preserves_liveness() {
        // SWAPN brings a deep dynamic value to TOS where RET_WORD consumes it.
        // CDL at bottom, 17 × PUSH0 above → SWAP 17 swaps TOS with CDL.
        let bytecode = with_dynamic_and_prefix(17, "SWAP 17 RET_WORD");
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped (consumed via SWAPN + MSTORE)"
        );
    }

    #[test]
    fn dupn_keeps_source_live() {
        // DUPN copies a deep dynamic value to TOS. RET_WORD consumes the copy.
        // CDL at bottom, 16 × PUSH0 above → DUP 17 copies CDL to TOS.
        let bytecode = with_dynamic_and_prefix(16, "DUP 17 RET_WORD");
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped"
        );
    }

    #[test]
    fn dupn_dead_copy() {
        // DUP copies a dynamic value, the copy is immediately POPped. The source
        // remains on the stack and feeds RET_WORD.
        let bytecode = analyze_asm("PUSH0 CALLDATALOAD DUP1 POP RET_WORD");
        assert!(
            bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP),
            "DUP1 should be skipped (copy is dead)"
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped"
        );
    }

    #[test]
    fn exchange_preserves_liveness() {
        // EXCHANGE swaps two non-TOS positions. SWAP2 brings the dynamic value
        // to TOS where RET_WORD consumes it.
        //
        // [0, CDL, 0]  → EXCHANGE(1,2) → [CDL, 0, 0]  → SWAP2 → [0, 0, CDL]
        let bytecode = analyze_asm_spec(
            "PUSH0 PUSH0 CALLDATALOAD PUSH0 EXCHANGE 1 2 SWAP2 RET_WORD",
            SpecId::AMSTERDAM,
        );
        assert!(
            !bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped (consumed via EXCHANGE + SWAP + MSTORE)"
        );
    }

    #[test]
    fn dup_dead() {
        // DUP1 with both source and copy dead should be eliminated along with its producer.
        let bytecode = analyze_asm(
            "
            PUSH1 0x42
            DUP1
            POP
            POP
            STOP
        ",
        );
        assert!(bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP));
        assert!(bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP));
    }

    #[test]
    fn swap_dead() {
        // SWAP1 with both positions dead should be eliminated.
        let bytecode = analyze_asm(
            "
            PUSH1 0x01
            PUSH1 0x02
            SWAP1
            POP
            POP
            STOP
        ",
        );
        for (i, name) in [(0, "PUSH 1"), (1, "PUSH 2"), (2, "SWAP1")] {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "{name} should be skipped"
            );
        }
    }

    #[test]
    fn swap_dead_with_live_passthrough() {
        // At SWAP2: TOS (pos 2) and pos 0 are both dead, but pos 1 (pass-through) is live.
        // The precise check correctly eliminates SWAP2. The generic out>0 check would
        // require all 3 output positions to be dead and miss this.
        // Uses CALLDATALOAD for dynamic values to prevent const-input DSE.
        let bytecode = analyze_asm(
            "
            PUSH0           ; inst 0
            CALLDATALOAD    ; inst 1: dynamic A
            PUSH1 0x20      ; inst 2
            CALLDATALOAD    ; inst 3: dynamic B
            PUSH1 0x40      ; inst 4
            CALLDATALOAD    ; inst 5: dynamic C
            SWAP2           ; inst 6: [C, B, A]
            POP             ; inst 7: [C, B]
            SWAP1           ; inst 8: [B, C]
            POP             ; inst 9: [B]
            RET_WORD        ; inst 10..15: return B
        ",
        );
        //       PUSH0, CDL_A, PUSH_20, CDL_B, PUSH_40, CDL_C, SWAP2, POP, SWAP1, POP
        for (i, alive) in [false, false, false, true, false, false, false, true, true, true]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                !bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                alive,
                "inst {i}"
            );
        }
    }

    #[test]
    fn returndatasize_dead() {
        let bytecode = analyze_asm(
            "
            RETURNDATASIZE
            POP
            STOP
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "RETURNDATASIZE should be skipped when dead"
        );
    }

    #[test]
    fn returndatasize_live() {
        // RETURNDATASIZE used as MSTORE offset — output is live, must NOT be eliminated.
        let bytecode = analyze_asm(
            "
            PUSH0
            RETURNDATASIZE
            MSTORE
            STOP
        ",
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "RETURNDATASIZE should NOT be skipped when live"
        );
    }

    #[test]
    fn blobhash_dead() {
        // BLOBHASH(0) with output popped — eliminable.
        let bytecode = analyze_asm(
            "
            PUSH0
            BLOBHASH
            POP
            STOP
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH0 should be skipped"
        );
        assert!(
            bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "BLOBHASH should be skipped when dead"
        );
    }

    #[test]
    fn blobhash_live() {
        // BLOBHASH(0) stored to memory — output is live.
        let bytecode = analyze_asm(
            "
            PUSH0
            BLOBHASH
            PUSH0
            MSTORE
            STOP
        ",
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "BLOBHASH should NOT be skipped when live"
        );
    }

    #[test]
    fn dupn_dead() {
        // 17 × PUSH0, DUP 17 (copies bottom), POP × 18, STOP — all dead.
        let pops = "POP\n".repeat(18);
        let bytecode = with_prefix(17, &format!("DUP 17 {pops} STOP"));
        for i in 0..17 {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "PUSH0 at {i} should be skipped"
            );
        }
        assert!(
            bytecode.inst(Inst::from_usize(17)).flags.contains(InstFlags::NOOP),
            "DUPN should be skipped"
        );
    }

    #[test]
    fn dupn_invalid_imm_not_eliminated() {
        // PUSH0, DUPN 0x5b (invalid immediate), POP, STOP.
        // The invalid immediate must NOT be eliminated — it should fail at runtime.
        // Raw bytes required: the asm parser rejects invalid immediates.
        let bytecode = analyze_code_spec(
            vec![op::PUSH0, op::DUPN, 0x5b, op::POP, op::STOP],
            SpecId::AMSTERDAM,
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "DUPN with invalid immediate should NOT be skipped"
        );
    }

    #[test]
    fn dupn_underflow_still_eliminated() {
        // 1 × PUSH0, DUP 17 (depth=17 but only 1 literal push), POP, POP, STOP.
        // Block analysis inflates stack_in to satisfy DUPN's real depth, so from DSE's
        // perspective the immediate is valid and the instruction is eliminable.
        let bytecode = with_prefix(1, "DUP 17 POP POP STOP");
        assert!(
            bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "DUPN with valid immediate should be skipped when dead"
        );
    }

    #[test]
    fn swapn_dead() {
        // 18 × PUSH0, SWAP 17 (depth=17), 18 × POP, STOP — all dead.
        let pops = "POP\n".repeat(18);
        let bytecode = with_prefix(18, &format!("SWAP 17 {pops} STOP"));
        for i in 0..18 {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "PUSH0 at {i} should be skipped"
            );
        }
        assert!(
            bytecode.inst(Inst::from_usize(18)).flags.contains(InstFlags::NOOP),
            "SWAPN should be skipped"
        );
    }

    #[test]
    fn exchange_dead() {
        // 3 × PUSH0, EXCHANGE (1,2), 3 × POP, STOP — all dead.
        let bytecode = with_prefix(3, "EXCHANGE 1 2 POP POP POP STOP");
        for i in 0..3 {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "PUSH0 at {i} should be skipped"
            );
        }
        assert!(
            bytecode.inst(Inst::from_usize(3)).flags.contains(InstFlags::NOOP),
            "EXCHANGE should be skipped"
        );
    }

    #[test]
    fn dupn_underflow_no_panic() {
        // DUP 17 with only 2 items on the stack — infeasible depth must not panic analysis.
        let _ = with_prefix(2, "DUP 17 STOP");
    }

    #[test]
    fn swapn_underflow_no_panic() {
        // SWAP 17 with only 2 items — infeasible depth must not panic analysis.
        let _ = with_prefix(2, "SWAP 17 STOP");
    }

    #[test]
    fn exchange_underflow_no_panic() {
        // EXCHANGE 1,2 with only 1 item — infeasible depth must not panic analysis.
        let _ = with_prefix(1, "EXCHANGE 1 2 STOP");
    }

    // --- Const-output dead store tests ---

    #[test]
    fn const_output_kills_all_inputs() {
        // ADD(2, 3) = 5: const_output is Some, so ALL inputs are dead.
        let bytecode = analyze_asm(
            "
            PUSH1 0x02      ; inst 0
            PUSH1 0x03      ; inst 1
            ADD             ; inst 2: folded to 5
            PUSH0           ; inst 3
            MSTORE          ; inst 4
            STOP            ; inst 5
        ",
        );
        for dead in [0, 1, 2, 3] {
            assert!(bytecode.inst(Inst::from_usize(dead)).flags.contains(InstFlags::NOOP));
        }
        for not_dead in [4, 5] {
            assert!(!bytecode.inst(Inst::from_usize(not_dead)).flags.contains(InstFlags::NOOP));
        }
    }

    #[test]
    fn const_output_chain() {
        // Chained folds: ADD(2,3)=5 feeds MUL(5,4)=20.
        // All four PUSHes and the ADD are dead.
        let bytecode = analyze_asm(
            "
            PUSH1 0x02      ; inst 0
            PUSH1 0x03      ; inst 1
            ADD             ; inst 2: folded to 5
            PUSH1 0x04      ; inst 3
            MUL             ; inst 4: folded to 20
            PUSH0           ; inst 5
            MSTORE          ; inst 6
            STOP            ; inst 7
        ",
        );
        for (i, name) in [(0, "PUSH 2"), (1, "PUSH 3"), (2, "ADD"), (3, "PUSH 4")] {
            assert!(
                bytecode.inst(Inst::from_usize(i)).flags.contains(InstFlags::NOOP),
                "{name} should be skipped"
            );
        }
    }

    #[test]
    fn const_input_add_dynamic() {
        // ADD(dynamic, 1): PUSH 1 is dead because ADD uses inline codegen (popn).
        let bytecode = analyze_asm(
            "
            PUSH0           ; inst 0: offset for CALLDATALOAD
            CALLDATALOAD    ; inst 1: dynamic value
            PUSH1 0x01      ; inst 2: constant addend
            ADD             ; inst 3: dynamic + 1
            PUSH0           ; inst 4
            MSTORE          ; inst 5
            STOP            ; inst 6
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP),
            "PUSH 1 (addend) should be skipped (const input)"
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped"
        );
    }

    #[test]
    fn const_input_add_zero() {
        // ADD(dynamic, 0): PUSH0 feeds the constant 0 input.
        let bytecode = analyze_asm(
            "
            PUSH0           ; inst 0: offset for CALLDATALOAD
            CALLDATALOAD    ; inst 1: dynamic value
            PUSH0           ; inst 2: constant 0
            ADD             ; inst 3: dynamic + 0
            PUSH0           ; inst 4
            MSTORE          ; inst 5
            STOP            ; inst 6
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP),
            "PUSH0 (addend) should be skipped (const input)"
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped"
        );
    }

    #[test]
    fn const_input_both_sides() {
        // ADD(0, dynamic): constant on the deeper side.
        let bytecode = analyze_asm(
            "
            PUSH0           ; inst 0: constant 0 (deeper input to ADD)
            PUSH0           ; inst 1: offset for CALLDATALOAD
            CALLDATALOAD    ; inst 2: dynamic value (TOS input to ADD)
            ADD             ; inst 3: 0 + dynamic
            PUSH0           ; inst 4
            MSTORE          ; inst 5
            STOP            ; inst 6
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH0 (deeper const input) should be skipped"
        );
        assert!(
            !bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped"
        );
    }

    #[test]
    fn builtin_const_input_killed() {
        // DIV(dynamic, 2): PUSH 2 feeds the divisor. DIV delegates to a builtin
        // (sp_after_inputs), but codegen pre-writes constant operands into the stack
        // slots, so DSE can safely kill the PUSH.
        let bytecode = analyze_asm(
            "
            PUSH0           ; inst 0: offset for CALLDATALOAD
            CALLDATALOAD    ; inst 1: dynamic value
            PUSH1 0x02      ; inst 2: constant divisor
            DIV             ; inst 3: dynamic / 2
            PUSH0           ; inst 4
            MSTORE          ; inst 5
            STOP            ; inst 6
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(2)).flags.contains(InstFlags::NOOP),
            "PUSH 2 (divisor) should be skipped (builtin const input)"
        );
        assert!(
            !bytecode.inst(Inst::from_usize(1)).flags.contains(InstFlags::NOOP),
            "CALLDATALOAD should NOT be skipped"
        );
    }

    #[test]
    fn sload_const_input_killed() {
        // SLOAD's input is a known constant — codegen pre-writes it via sp_after_inputs,
        // so the PUSH is dead.
        let bytecode = analyze_asm(
            "
            PUSH1 0x00      ; inst 0: storage key
            SLOAD           ; inst 1: read storage
            PUSH0           ; inst 2
            MSTORE          ; inst 3
            STOP            ; inst 4
        ",
        );
        assert!(
            bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH 0 should be skipped (const input)"
        );
    }

    #[test]
    fn const_live_across_suspend_not_nooped() {
        // PUSH1 0xaa is live across a CALL suspension: after the child returns,
        // POP drops the success flag and MSTORE writes 0xaa. DSE must not NOOP
        // the PUSH because copy_stack_to_arg copies the stack prefix at suspend
        // time before any consumer rematerializes the constant.
        let bytecode = analyze_asm(
            "
            PUSH1 0xaa      ; inst 0: live across CALL
            PUSH0           ; inst 1
            PUSH0           ; inst 2
            PUSH0           ; inst 3
            PUSH0           ; inst 4
            PUSH0           ; inst 5
            PUSH1 0x69      ; inst 6
            GAS             ; inst 7
            CALL            ; inst 8: suspends
            POP             ; inst 9
            PUSH0           ; inst 10
            MSTORE          ; inst 11
            PUSH1 0x20      ; inst 12
            PUSH0           ; inst 13
            RETURN          ; inst 14
        ",
        );
        assert!(
            !bytecode.inst(Inst::from_usize(0)).flags.contains(InstFlags::NOOP),
            "PUSH1 0xaa must NOT be nooped — it is live across the CALL suspension"
        );
    }
}
