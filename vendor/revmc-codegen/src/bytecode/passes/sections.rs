use core::fmt;

use revm_bytecode::opcode as op;
use revm_primitives::U256;

use crate::bytecode::{Bytecode, Inst, InstFlags};

/// A gas section tracks the total base gas cost of a sequence of instructions.
///
/// Gas sections end at instructions that require `gasleft` (e.g. `GAS`, `SSTORE`),
/// branching instructions, or suspending instructions.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GasSection {
    /// The total base gas cost of all instructions in the section.
    pub(crate) gas_cost: u32,
}

impl fmt::Debug for GasSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            f.write_str("GasSection::EMPTY")
        } else {
            f.debug_struct("GasSection").field("gas_cost", &self.gas_cost).finish()
        }
    }
}

impl GasSection {
    /// Returns `true` if the section is empty.
    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// A stack section tracks stack height requirements for a sequence of instructions.
///
/// Stack sections end at instructions that require `gasleft`, branching instructions, or
/// suspending instructions. This keeps stack failures after dynamic-gas and host-effect
/// instructions from being hoisted ahead of those instructions.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StackSection {
    /// The stack height required to execute the section.
    pub(crate) inputs: u16,
    /// The maximum stack height growth relative to the stack height at section start.
    pub(crate) max_growth: i16,
}

impl fmt::Debug for StackSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            f.write_str("StackSection::EMPTY")
        } else {
            f.debug_struct("StackSection")
                .field("inputs", &self.inputs)
                .field("max_growth", &self.max_growth)
                .finish()
        }
    }
}

impl StackSection {
    /// Returns `true` if the section is empty.
    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// Compute a stack section from an iterator of `(inputs, outputs)` pairs.
    pub(crate) fn from_stack_io(iter: impl IntoIterator<Item = (u8, u8)>) -> Self {
        let mut analysis = StackSectionAnalysis::default();
        for (inp, out) in iter {
            analysis.process(inp, out);
        }
        analysis.section()
    }
}

/// Gas section analysis state.
pub(crate) struct GasSectionAnalysis {
    gas_cost: u64,
    start_inst: Inst,
}

impl Default for GasSectionAnalysis {
    fn default() -> Self {
        Self { gas_cost: 0, start_inst: Inst::from_usize(0) }
    }
}

impl GasSectionAnalysis {
    /// Accumulates a base gas cost.
    #[inline]
    pub(crate) fn process(&mut self, base_gas: u16) {
        self.gas_cost += base_gas as u64;
    }

    /// Accumulates extra gas (e.g. pre-computed dynamic gas for folded instructions).
    #[inline]
    pub(crate) fn process_extra(&mut self, gas: u64) {
        self.gas_cost += gas;
    }

    fn save_to_reset(&mut self, bytecode: &mut Bytecode<'_>, next_section_inst: Inst) {
        self.save_to(bytecode, next_section_inst);
        self.reset(next_section_inst);
    }

    /// Saves the current gas section to the bytecode.
    fn save_to(&self, bytecode: &mut Bytecode<'_>, next_section_inst: Inst) {
        if self.start_inst >= bytecode.insts.len_idx() {
            return;
        }
        let section = self.section();
        if !section.is_empty() {
            let _ = next_section_inst;
            // trace!(
            //     inst = %self.start_inst,
            //     len = %(next_section_inst - self.start_inst).0,
            //     ?section,
            //     "saving gas"
            // );
            let mut insts = bytecode.insts[self.start_inst..].iter_mut();
            if let Some(inst) = insts.find(|inst| !inst.is_dead_code()) {
                inst.gas_section = section;
            }
        }
    }

    /// Resets the analysis for a new section.
    pub(crate) fn reset(&mut self, inst: Inst) {
        *self = Self { start_inst: inst, ..Default::default() };
    }

    /// Returns the current gas section.
    pub(crate) fn section(&self) -> GasSection {
        GasSection { gas_cost: self.gas_cost.try_into().unwrap_or(u32::MAX) }
    }
}

/// Stack section analysis state.
pub(crate) struct StackSectionAnalysis {
    inputs: i32,
    diff: i32,
    max_growth: i32,
    start_inst: Inst,
}

impl Default for StackSectionAnalysis {
    fn default() -> Self {
        Self { inputs: 0, diff: 0, max_growth: 0, start_inst: Inst::from_usize(0) }
    }
}

impl StackSectionAnalysis {
    /// Returns the cumulative stack delta so far.
    #[inline]
    pub(crate) fn diff(&self) -> i32 {
        self.diff
    }

    /// Accumulates a single instruction's stack I/O.
    #[inline]
    pub(crate) fn process(&mut self, inputs: u8, outputs: u8) {
        self.inputs = self.inputs.max(inputs as i32 - self.diff);
        self.diff += outputs as i32 - inputs as i32;
        self.max_growth = self.max_growth.max(self.diff);
    }

    fn save_to_reset(&mut self, bytecode: &mut Bytecode<'_>, next_section_inst: Inst) {
        self.save_to(bytecode, next_section_inst);
        self.reset(next_section_inst);
    }

    /// Saves the current stack section to the bytecode.
    fn save_to(&self, bytecode: &mut Bytecode<'_>, next_section_inst: Inst) {
        if self.start_inst >= bytecode.insts.len_idx() {
            return;
        }
        let section = self.section();
        let _ = next_section_inst;
        // trace!(
        //     inst = %self.start_inst,
        //     len = %(next_section_inst - self.start_inst).0,
        //     ?section,
        //     "saving stack"
        // );
        let mut insts = bytecode.insts[self.start_inst..].iter_mut();
        if let Some(inst) = insts.find(|inst| !inst.is_dead_code()) {
            inst.flags |= InstFlags::STACK_SECTION_HEAD;
            inst.stack_section = section;
        }
    }

    /// Resets the analysis for a new section.
    pub(crate) fn reset(&mut self, inst: Inst) {
        *self = Self { start_inst: inst, ..Default::default() };
    }

    /// Returns the current stack section.
    pub(crate) fn section(&self) -> StackSection {
        StackSection {
            inputs: self.inputs.try_into().unwrap_or(u16::MAX),
            max_growth: self.max_growth.try_into().unwrap_or(i16::MAX),
        }
    }
}

/// Instruction section analysis, tracking gas and stack sections separately.
#[derive(Default)]
pub(crate) struct SectionsAnalysis {
    gas: GasSectionAnalysis,
    stack: StackSectionAnalysis,
}

impl SectionsAnalysis {
    /// Process a single instruction.
    pub(crate) fn process(&mut self, bytecode: &mut Bytecode<'_>, inst: Inst) {
        // JUMPDEST starts both gas and stack sections.
        if bytecode.inst(inst).is_reachable_jumpdest(bytecode.has_dynamic_jumps()) {
            self.stack.save_to_reset(bytecode, inst);
            self.gas.save_to_reset(bytecode, inst);
        }

        let data = bytecode.inst(inst);
        let (inp, out) = data.stack_io();
        self.stack.process(inp, out);
        self.gas.process(data.base_gas);

        // When EXP's builtin will be skipped — either because both operands are known
        // (const_output) or because the exponent is known and handled by a peephole (≤ 2) —
        // fold the dynamic gas into the section statically.
        if data.opcode == op::EXP
            && let Some(exponent) = bytecode.const_operand(inst, 1)
            && (bytecode.const_output(inst).is_some() || exponent <= U256::from(2))
        {
            self.gas.process_extra(bytecode.gas_params.exp_cost(exponent));
        }

        // Dynamic-gas, branching, and suspending instructions end both sections.
        let next = inst + 1;
        if data.may_suspend() || data.is_branching() || data.requires_gasleft(bytecode.spec_id) {
            self.stack.save_to_reset(bytecode, next);
            self.gas.save_to_reset(bytecode, next);
        }
    }

    /// Finishes the analysis.
    pub(crate) fn finish(self, bytecode: &mut Bytecode<'_>) {
        let last = bytecode.insts.len_idx() - 1;
        self.gas.save_to(bytecode, last);
        self.stack.save_to(bytecode, last);
        if enabled!(tracing::Level::DEBUG) {
            let mut max_len = 0usize;
            let mut count = 0usize;
            let mut section_len = 0usize;
            for (_inst, data) in bytecode.iter_insts() {
                section_len += 1;
                if !data.is_stack_section_head() {
                    continue;
                }
                // `section_len` includes this head; the previous section had `section_len - 1`.
                if count > 0 {
                    max_len = max_len.max(section_len - 1);
                }
                section_len = 1;
                count += 1;
            }
            // Account for the last section.
            max_len = max_len.max(section_len);
            debug!(count, max_len, "sections");
        }
    }
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;
    use revm_primitives::hardfork::SpecId;

    use crate::bytecode::{
        Inst,
        passes::block_analysis::tests::{analyze_asm, analyze_asm_spec, analyze_code_spec},
    };

    /// Returns the gas section cost for the first non-dead instruction (the section head).
    fn section_gas(src: &str) -> u32 {
        let bytecode = analyze_asm(src);
        bytecode.inst(Inst::from_usize(0)).gas_section.gas_cost
    }

    /// EXP with an asymmetric base/exponent: base is 1 byte (2), exponent is 2 bytes (256).
    /// If the wrong operand were used for gas, the dynamic cost would be 50*1=50 instead of
    /// 50*2=100, and the total would differ.
    #[test]
    fn exp_folded_gas_uses_exponent_not_base() {
        // PUSH 256, PUSH 2, EXP, PUSH0, MSTORE, STOP
        // Gas: PUSH(3) + PUSH(3) + EXP_base(10) + EXP_dynamic(50*2=100) + PUSH0(2) + MSTORE(3) +
        // STOP(0)    = 3 + 3 + 10 + 100 + 2 + 3 + 0 = 121
        let gas = section_gas("PUSH 256 PUSH 2 EXP PUSH0 MSTORE STOP");
        assert_eq!(gas, 121, "dynamic gas must use exponent (256=2 bytes), not base (2=1 byte)");
    }

    /// EXP with zero exponent has no dynamic gas.
    #[test]
    fn exp_folded_gas_zero_exponent() {
        // PUSH0(2) + PUSH(3) + EXP_base(10) + EXP_dynamic(0) + PUSH0(2) + MSTORE(3) + STOP(0) = 20
        let gas = section_gas("PUSH 0 PUSH 2 EXP PUSH0 MSTORE STOP");
        assert_eq!(gas, 20, "zero exponent should have no dynamic gas");
    }

    /// EXP with max exponent (32 bytes).
    #[test]
    fn exp_folded_gas_max_exponent() {
        // PUSH U256::MAX (32 bytes), PUSH 2, EXP, PUSH0, MSTORE, STOP
        // Gas: PUSH(3) + PUSH(3) + EXP_base(10) + EXP_dynamic(50*32=1600) + PUSH0(2) + MSTORE(3) +
        // STOP(0)    = 3 + 3 + 10 + 1600 + 2 + 3 + 0 = 1621
        let gas = section_gas(
            "PUSH 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff \
             PUSH 2 EXP PUSH0 MSTORE STOP",
        );
        assert_eq!(gas, 1621, "max exponent should cost 50*32=1600 dynamic gas");
    }

    /// Disabled opcodes must not contribute stack I/O or gas to the preceding section.
    /// Otherwise the section-head underflow check fires before the disabled opcode's
    /// `NotActivated` guard, producing a divergent `StackUnderflow`.
    #[test]
    fn disabled_opcode_does_not_poison_section() {
        // CALLDATASIZE(0→1) ; TSTORE(2→0, disabled before Cancun)
        let bytecode = analyze_asm_spec("CALLDATASIZE TSTORE", SpecId::SHANGHAI);
        let head = bytecode.inst(Inst::from_usize(0));
        assert_eq!(
            head.stack_section.inputs, 0,
            "CALLDATASIZE needs 0 inputs; disabled TSTORE must not inflate the section"
        );
        assert_eq!(head.gas_section.gas_cost, 2, "only CALLDATASIZE gas (2) should be charged");
    }

    /// DUPN with an invalid immediate must have stack_io = (0, 0) so that section analysis
    /// matches translate (which emits `InvalidImmediateEncoding` and diverges).
    /// Previously, the fallback to OPCODE_INFO's placeholder `(0, 1)` inflated the section's
    /// cumulative diff, causing translate to underflow the virtual stack on a subsequent SSTORE.
    #[test]
    fn invalid_dupn_imm_does_not_inflate_section() {
        // PUSH0, DUPN 0x5b (invalid immediate), PUSH0, SSTORE, STOP
        // Without the fix, DUPN had stack_io=(0,1), inflating the section diff by 1.
        // Translate would skip the +1 (goto_return!(fail)), then SSTORE's diff=-2 would
        // underflow the virtual stack.
        let bytecode = analyze_code_spec(
            vec![op::PUSH0, op::DUPN, 0x5b, op::PUSH0, op::SSTORE, op::STOP],
            SpecId::AMSTERDAM,
        );
        let dupn = bytecode.inst(Inst::from_usize(1));
        assert_eq!(dupn.stack_io(), (0, 0), "DUPN with invalid immediate must have zero stack I/O");
    }
}
