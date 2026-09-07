//! Peephole optimizations applied during translation.
//!
//! These fire when abstract interpretation has proven one or more operands are constant,
//! replacing expensive opaque builtins (DIV, MOD, SDIV, SMOD, ADDMOD, MULMOD, EXP) with native
//! LLVM operations that it can optimize further (e.g. pow2 udiv → lshr, pow2 urem → and).

use revm_bytecode::opcode as op;
use revm_interpreter::InstructionResult;
use revm_primitives::U256;
use revmc_builtins::Builtin;

use super::FunctionCx;
use crate::{Backend, Builder, InstData, IntCC};

/// i256 INT_MIN: 1 << 255.
const INT_MIN: U256 = U256::ONE.wrapping_shl(255);

impl<'a, B: Backend> FunctionCx<'a, B> {
    /// Tries to emit optimized inline code for an instruction whose operands are partially known.
    ///
    /// Returns `true` if the peephole fired and code was emitted, `false` to fall through to the
    /// normal translation.
    pub(super) fn try_peephole(&mut self, data: &InstData) -> bool {
        match data.opcode {
            op::DIV => self.peephole_div(),
            op::SDIV => self.peephole_sdiv(),
            op::MOD => self.peephole_mod(),
            op::SMOD => self.peephole_smod(),
            op::ADDMOD => self.peephole_addmod(),
            op::MULMOD => self.peephole_mulmod(),
            op::EXP => self.peephole_exp(),
            op::SIGNEXTEND => self.peephole_signextend(),
            op::BYTE => self.peephole_byte(),

            op::CALLDATALOAD | op::SLOAD => self.peephole_load(data.opcode),

            op::KECCAK256 => self.peephole_keccak256(),
            op::RETURN | op::REVERT => self.peephole_return(data.opcode),

            _ => false,
        }
    }

    /// DIV a, b => a / b.
    ///
    /// General constant divisors could use native LLVM `udiv`, but i256 division
    /// generates very bloated code (~100+ instructions for the reciprocal multiply),
    /// so we only emit native ops for powers of two where LLVM lowers to `lshr`.
    fn peephole_div(&mut self) -> bool {
        let [dividend, divisor] = self.const_operands();
        match divisor {
            // x / 0 => 0.
            Some(U256::ZERO) => self.fold_const(0),
            // x / 1 => x.
            Some(U256::ONE) => {
                let [a, _] = self.popn();
                self.push(a);
            }
            // x / C (pow2) => native udiv, LLVM lowers to lshr.
            Some(d) if d.is_power_of_two() => {
                let [a, _] = self.popn();
                let d = self.bcx.iconst_256(d);
                let r = self.bcx.udiv(a, d);
                self.push(r);
            }
            // 0 / x => 0 (EVM: 0 / 0 = 0).
            _ if dividend == Some(U256::ZERO) => self.fold_const(0),
            _ => return false,
        }
        true
    }

    /// SDIV a, b => signed(a) / signed(b), rounded toward zero.
    ///
    /// Pow2 cases are intentionally skipped: signed division rounds toward zero while
    /// arithmetic shift right rounds toward negative infinity, so the semantics differ.
    fn peephole_sdiv(&mut self) -> bool {
        let [dividend, divisor] = self.const_operands();
        match divisor {
            // x / 0 => 0.
            Some(U256::ZERO) => self.fold_const(0),
            // x / 1 => x.
            Some(U256::ONE) => {
                let [a, _] = self.popn();
                self.push(a);
            }
            // x / -1 => -x.
            Some(U256::MAX) => {
                let [a, _] = self.popn();
                let zero = self.bcx.iconst_256(0);
                let r = self.bcx.isub(zero, a);
                self.push(r);
            }
            // x / INT_MIN => zext(x == INT_MIN).
            Some(INT_MIN) => {
                let [a, _] = self.popn();
                let min = self.bcx.iconst_256(INT_MIN);
                let cmp = self.bcx.icmp(IntCC::Equal, a, min);
                let r = self.bcx.zext(self.word_type, cmp);
                self.push(r);
            }
            // 0 / x => 0 (EVM: 0 / 0 = 0).
            _ if dividend == Some(U256::ZERO) => self.fold_const(0),
            _ => return false,
        }
        true
    }

    /// MOD a, b => a % b.
    ///
    /// Same as DIV: only pow2 moduli use native `urem` (LLVM lowers to `and`).
    fn peephole_mod(&mut self) -> bool {
        let [dividend, modulus] = self.const_operands();
        match modulus {
            // x % 0 => 0, x % 1 => 0.
            Some(U256::ZERO) | Some(U256::ONE) => self.fold_const(0),
            // x % C (pow2) => native urem, LLVM lowers to and.
            Some(m) if m.is_power_of_two() => {
                let [a, _] = self.popn();
                let m = self.bcx.iconst_256(m);
                let r = self.bcx.urem(a, m);
                self.push(r);
            }
            // 0 % x => 0 (EVM: 0 % 0 = 0).
            _ if dividend == Some(U256::ZERO) => self.fold_const(0),
            _ => return false,
        }
        true
    }

    /// SMOD a, b => signed(a) % signed(b). Result sign matches dividend.
    fn peephole_smod(&mut self) -> bool {
        let [dividend, modulus] = self.const_operands();
        match modulus {
            // x % 0 => 0, x % 1 => 0, x % -1 => 0.
            Some(U256::ZERO) | Some(U256::ONE) | Some(U256::MAX) => self.fold_const(0),
            // 0 % x => 0 (EVM: 0 % 0 = 0).
            _ if dividend == Some(U256::ZERO) => self.fold_const(0),
            _ => return false,
        }
        true
    }

    /// ADDMOD a, b, N => (a + b) % N.
    fn peephole_addmod(&mut self) -> bool {
        let [a, b, modulus] = self.const_operands();
        match modulus {
            // (a + b) % 0 => 0, (a + b) % 1 => 0.
            Some(U256::ZERO) | Some(U256::ONE) => self.fold_const(0),
            _ => match (a, b) {
                // (0 + 0) % N => 0.
                (Some(U256::ZERO), Some(U256::ZERO)) => self.fold_const(0),
                _ => return false,
            },
        }
        true
    }

    /// MULMOD a, b, N => (a * b) % N.
    fn peephole_mulmod(&mut self) -> bool {
        let [a, b, modulus] = self.const_operands();
        match modulus {
            // (a * b) % 0 => 0, (a * b) % 1 => 0.
            Some(U256::ZERO) | Some(U256::ONE) => self.fold_const(0),
            _ => {
                // a * 0 => 0, 0 * b => 0.
                if a == Some(U256::ZERO) || b == Some(U256::ZERO) {
                    self.fold_const(0);
                } else {
                    return false;
                }
            }
        }
        true
    }

    /// EXP base, exp => base ** exp.
    ///
    /// Dynamic gas is folded into the section gas cost by `SectionsAnalysis` when the
    /// exponent is a compile-time constant, so the section gas check already covers it.
    /// For constant-base cases with a dynamic exponent, call `ExpGas` to charge only the
    /// dynamic gas and compute the result inline.
    fn peephole_exp(&mut self) -> bool {
        let [base, exponent] = self.const_operands();
        match exponent {
            // x ** 0 => 1.
            Some(U256::ZERO) => self.fold_const(1),
            // x ** 1 => x.
            Some(U256::ONE) => {
                let [a, _] = self.popn();
                self.push(a);
            }
            // x ** 2 => x * x.
            Some(e) if e == 2 => {
                let [a, _] = self.popn();
                let r = self.bcx.imul(a, a);
                self.push(r);
            }
            Some(_) => return false,
            None => {}
        }
        if exponent.is_some() {
            return true;
        }
        match base {
            // 0 ** x => x == 0 ? 1 : 0.
            Some(U256::ZERO) => {
                self.pay_exp_dynamic_gas();
                let [_, exponent] = self.popn();
                let is_zero = self.bcx.icmp_imm(IntCC::Equal, exponent, 0);
                let one = self.bcx.iconst_256(1);
                let zero = self.bcx.iconst_256(0);
                let r = self.bcx.select(is_zero, one, zero);
                self.push(r);
            }
            // 1 ** x => 1.
            Some(U256::ONE) => {
                self.pay_exp_dynamic_gas();
                self.fold_const(1);
            }
            // (-1) ** x => x % 2 == 0 ? 1 : -1.
            Some(U256::MAX) => {
                self.pay_exp_dynamic_gas();
                let [_, exponent] = self.popn();
                let is_odd = self.bcx.bitand_imm(exponent, 1);
                let is_even = self.bcx.icmp_imm(IntCC::Equal, is_odd, 0);
                let one = self.bcx.iconst_256(1);
                let minus_one = self.bcx.iconst_256(U256::MAX);
                let r = self.bcx.select(is_even, one, minus_one);
                self.push(r);
            }
            // (2 ** k) ** x => x < ceil(256 / k) ? 1 << (k * x) : 0.
            Some(base) if base.is_power_of_two() => {
                self.pay_exp_dynamic_gas();
                let k = base.trailing_zeros();
                let threshold = i64::from(256_u16.div_ceil(k as u16));
                let [_, exponent] = self.popn();
                let in_range = self.bcx.icmp_imm(IntCC::UnsignedLessThan, exponent, threshold);
                let shift = if k == 1 { exponent } else { self.bcx.imul_imm(exponent, k as i64) };
                let one = self.bcx.iconst_256(1);
                let shifted = self.bcx.ishl(one, shift);
                let zero = self.bcx.iconst_256(0);
                let r = self.bcx.select(in_range, shifted, zero);
                self.push(r);
            }
            _ => return false,
        }
        true
    }

    fn pay_exp_dynamic_gas(&mut self) {
        let exponent = self.sp_after_inputs_with(&[1]);
        self.call_fallible_builtin(Builtin::ExpGas, &[self.ecx, exponent]);
    }

    /// SIGNEXTEND ext, x => sign-extend x from (ext+1) bytes.
    fn peephole_signextend(&mut self) -> bool {
        let [ext, _x] = self.const_operands();
        match ext {
            // SIGNEXTEND(e, x) with e >= 31 => x (no-op, value already fills 32 bytes).
            Some(e) if e >= 31 => {
                let [_, x] = self.popn();
                self.push(x);
            }
            _ => return false,
        }
        true
    }

    /// BYTE index, value => `value[index]`.
    fn peephole_byte(&mut self) -> bool {
        let [index, _value] = self.const_operands();
        match index {
            // BYTE(i, x) with i >= 32 => 0.
            Some(i) if i >= 32 => self.fold_const(0),
            _ => return false,
        }
        true
    }

    fn peephole_load(&mut self, op: u8) -> bool {
        if let [Some(offset)] = self.const_operands()
            && let Ok(offset) = u64::try_from(offset)
        {
            let builtin = match op {
                op::SLOAD => Builtin::SloadC,
                op::CALLDATALOAD => Builtin::CallDataLoadC,
                _ => unreachable!(),
            };
            let sp = self.sp_from_top(1);
            let offset = self.bcx.iconst(self.isize_type, offset as i64);
            let args = &[self.ecx, sp, offset];
            if op == op::CALLDATALOAD {
                let _ = self.call_builtin(builtin, args);
            } else {
                self.call_fallible_builtin(builtin, args);
            }
            // Builtin wrote output to sp; reload into virtual stack.
            let off = self.section_len_offset - 1;
            let value = self.load_word(sp, "builtin.out");
            self.vstack.set_at_offset(off, value);
            true
        } else {
            false
        }
    }

    fn peephole_keccak256(&mut self) -> bool {
        if let Some((offset, len)) = self.const_memory_operands(self.const_operands()) {
            let offset = self.bcx.iconst(self.isize_type, offset as i64);
            let len = self.bcx.iconst(self.isize_type, len as i64);
            let sp = self.sp_after_inputs_with(&[]);
            self.call_fallible_builtin(Builtin::Keccak256CC, &[self.ecx, sp, offset, len]);
            true
        } else {
            false
        }
    }

    fn peephole_return(&mut self, op: u8) -> bool {
        if let Some((offset, len)) = self.const_memory_operands(self.const_operands()) {
            // Both operands constant; consume from virtual stack.
            self.pop_ignore(2);
            let ir = match op {
                op::RETURN => InstructionResult::Return,
                op::REVERT => InstructionResult::Revert,
                _ => unreachable!(),
            };
            let ir_const = self.bcx.iconst(self.i8_type, ir as i64);
            let offset = self.bcx.iconst(self.isize_type, offset as i64);
            let len = self.bcx.iconst(self.isize_type, len as i64);
            let _ = self.call_builtin(Builtin::DoReturnCC, &[self.ecx, offset, len, ir_const]);
            self.bcx.unreachable();
            true
        } else {
            false
        }
    }

    /* utils */

    fn const_memory_operands(&self, args: [Option<U256>; 2]) -> Option<(u64, u64)> {
        if let [offset, Some(len)] = args
            && let Ok(len) = u64::try_from(len)
            // For len == 0 offset is ignored.
            && let Some(offset) = offset
                .and_then(|offset| u64::try_from(offset).ok())
                .or_else(|| (len == 0).then_some(0))
        {
            Some((offset, len))
        } else {
            None
        }
    }
}
