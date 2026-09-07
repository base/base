//! Constant folding for EVM arithmetic during abstract interpretation.

use std::cmp::Ordering;

use revm_bytecode::opcode as op;
use revm_interpreter::instructions::i256::{i256_cmp, i256_div, i256_mod};
use revm_primitives::U256;

use super::block_analysis::AbsValue;
use crate::{
    InstData,
    bytecode::{U256Imm, U256Interner},
};

/// Returns the compiler gas cost of constant-folding the given opcode with the given inputs.
///
/// Uses the real EVM gas schedule: most arithmetic is cheap (3–5 gas), but `EXP` costs
/// `10 + 50 * byte_size(exponent)` which can be weaponised with large exponents.
///
/// Returns `None` for opcodes that `try_const_fold` does not handle.
pub(crate) fn const_fold_gas(
    opcode: u8,
    inputs: &[AbsValue],
    interner: &U256Interner,
) -> Option<u64> {
    let gas: u64 = match opcode {
        op::CODESIZE | op::PC => 2,
        op::ISZERO | op::NOT => 3,
        op::CLZ => 3,
        op::ADD | op::SUB => 3,
        op::MUL | op::DIV | op::SDIV | op::MOD | op::SMOD => 5,
        op::ADDMOD | op::MULMOD => 8,
        op::SIGNEXTEND => 5,
        op::LT | op::GT | op::SLT | op::SGT | op::EQ => 3,
        op::AND | op::OR | op::XOR => 3,
        op::BYTE | op::SHL | op::SHR | op::SAR => 3,
        op::EXP => {
            // EXP: 10 + 50 * byte_size(exponent).
            // The exponent is the second operand (TOS - 1 in EVM stack order, inputs[0] here).
            let exponent_bytes = match inputs.first() {
                Some(&AbsValue::Const(imm)) => {
                    let val = imm.get(interner);
                    (256 - val.leading_zeros()).div_ceil(8)
                }
                _ => return None,
            };
            10 + 50 * exponent_bytes as u64
        }
        _ => return None,
    };
    Some(gas)
}

/// Try to constant-fold an instruction.
///
/// `code_len` is the length of the bytecode (for `CODESIZE`).
pub(crate) fn try_const_fold(
    inst: &InstData,
    inputs: &[AbsValue],
    interner: &mut U256Interner,
    code_len: usize,
) -> Option<AbsValue> {
    let opcode = inst.opcode;
    let result = match opcode {
        // 0 -> 1
        op::CODESIZE => U256::from(code_len),
        op::PC => U256::from(inst.pc_imm()),

        // 1 -> 1
        op::ISZERO | op::NOT | op::CLZ => {
            let &[AbsValue::Const(ai)] = inputs else {
                return None;
            };
            let a = ai.get(interner);
            match opcode {
                op::ISZERO => U256::from(a.is_zero()),
                op::NOT => !a,
                op::CLZ => U256::from(a.leading_zeros()),
                _ => unreachable!(),
            }
        }

        // 2 -> 1
        op::ADD
        | op::MUL
        | op::SUB
        | op::DIV
        | op::SDIV
        | op::MOD
        | op::SMOD
        | op::EXP
        | op::SIGNEXTEND
        | op::LT
        | op::GT
        | op::SLT
        | op::SGT
        | op::EQ
        | op::AND
        | op::OR
        | op::XOR
        | op::BYTE
        | op::SHL
        | op::SHR
        | op::SAR => {
            let &[AbsValue::Const(bi), AbsValue::Const(ai)] = inputs else {
                return None;
            };
            let a = ai.get(interner);
            let b = bi.get(interner);
            match opcode {
                op::ADD => a.wrapping_add(b),
                op::MUL => a.wrapping_mul(b),
                op::SUB => a.wrapping_sub(b),
                op::DIV => {
                    if !b.is_zero() {
                        a.wrapping_div(b)
                    } else {
                        U256::ZERO
                    }
                }
                op::SDIV => i256_div(a, b),
                op::MOD => {
                    if !b.is_zero() {
                        a.wrapping_rem(b)
                    } else {
                        U256::ZERO
                    }
                }
                op::SMOD => i256_mod(a, b),
                op::EXP => a.pow(b),
                op::SIGNEXTEND => {
                    if a < U256::from(31) {
                        let ext = a.as_limbs()[0];
                        let bit_index = (8 * ext + 7) as usize;
                        let bit = b.bit(bit_index);
                        let mask = (U256::from(1) << bit_index) - U256::from(1);
                        if bit { b | !mask } else { b & mask }
                    } else {
                        b
                    }
                }
                op::LT => U256::from(a < b),
                op::GT => U256::from(a > b),
                op::SLT => U256::from(i256_cmp(&a, &b) == Ordering::Less),
                op::SGT => U256::from(i256_cmp(&a, &b) == Ordering::Greater),
                op::EQ => U256::from(a == b),
                op::AND => a & b,
                op::OR => a | b,
                op::XOR => a ^ b,
                op::BYTE => {
                    let i = a.saturating_to::<usize>();
                    if i < 32 { U256::from(b.byte(31 - i)) } else { U256::ZERO }
                }
                op::SHL => {
                    let shift = a.saturating_to::<usize>();
                    if shift < 256 { b << shift } else { U256::ZERO }
                }
                op::SHR => {
                    let shift = a.saturating_to::<usize>();
                    if shift < 256 { b >> shift } else { U256::ZERO }
                }
                op::SAR => {
                    let shift = a.saturating_to::<usize>();
                    if shift < 256 {
                        b.arithmetic_shr(shift)
                    } else if b.bit(255) {
                        U256::MAX
                    } else {
                        U256::ZERO
                    }
                }
                _ => unreachable!(),
            }
        }

        // 3 -> 1
        op::ADDMOD | op::MULMOD => {
            let &[AbsValue::Const(ci), AbsValue::Const(bi), AbsValue::Const(ai)] = inputs else {
                return None;
            };
            let a = ai.get(interner);
            let b = bi.get(interner);
            let n = ci.get(interner);
            match opcode {
                op::ADDMOD => a.add_mod(b, n),
                op::MULMOD => a.mul_mod(b, n),
                _ => unreachable!(),
            }
        }

        _ => return None,
    };
    debug_assert!(
        const_fold_gas(inst.opcode, inputs, interner).is_some(),
        "try_const_fold handled opcode {} but const_fold_gas did not",
        inst.opcode,
    );
    Some(AbsValue::Const(U256Imm::new(result, interner)))
}

#[cfg(test)]
mod tests {
    use std::{fmt::Write, time::Instant};

    use revm_bytecode::opcode::OpCode;
    use revm_primitives::U256;

    use super::*;
    use crate::bytecode::{
        Bytecode,
        passes::block_analysis::tests::{Inst, analyze_asm},
    };

    /// Builds bytecode that pushes operands, executes `opcode`, sinks the result
    /// with `PUSH0 MSTORE STOP`, analyzes it, and returns the folded result.
    ///
    /// Operands are in natural order matching the yellow paper specification:
    /// for `SUB(a, b)` = `a - b`, pass `&[a, b]` (first popped = first element).
    fn const_fold(opcode: u8, operands: &[U256]) -> Option<U256> {
        use std::fmt::Write;
        let mut src = String::new();
        let mut num_insts = 0usize;
        // Push in reverse so that operands[0] ends up on TOS.
        for v in operands.iter().rev() {
            writeln!(src, "PUSH {v}").unwrap();
            num_insts += 1;
        }
        writeln!(src, "{}", OpCode::new(opcode).unwrap()).unwrap();
        num_insts += 1;
        // Sink: PUSH0 + MSTORE + STOP to consume the result.
        let mstore_inst = num_insts + 1;
        writeln!(src, "PUSH0\nMSTORE\nSTOP").unwrap();

        let bytecode = analyze_asm(&src);
        // The folded result is operand 1 (second from top) at MSTORE.
        bytecode.const_operand(Inst::from_usize(mstore_inst), 1)
    }

    #[test]
    fn const_fold_0_to_1() {
        // CODESIZE: code = CODESIZE(1) + PUSH0(1) + MSTORE(1) + STOP(1) = 4 bytes.
        assert_eq!(const_fold(op::CODESIZE, &[]), Some(U256::from(4)));

        // PC: at position 0.
        assert_eq!(const_fold(op::PC, &[]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_1_to_1() {
        // ISZERO.
        assert_eq!(const_fold(op::ISZERO, &[U256::ZERO]), Some(U256::from(1)));
        assert_eq!(const_fold(op::ISZERO, &[U256::from(42)]), Some(U256::ZERO));

        // NOT.
        assert_eq!(const_fold(op::NOT, &[U256::ZERO]), Some(U256::MAX));
        assert_eq!(const_fold(op::NOT, &[U256::MAX]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_add() {
        assert_eq!(const_fold(op::ADD, &[U256::from(3), U256::from(4)]), Some(U256::from(7)));
        assert_eq!(const_fold(op::ADD, &[U256::MAX, U256::from(1)]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_mul() {
        assert_eq!(const_fold(op::MUL, &[U256::from(3), U256::from(7)]), Some(U256::from(21)));
        assert_eq!(const_fold(op::MUL, &[U256::from(5), U256::ZERO]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_sub() {
        // SUB(10, 3) = 7.
        assert_eq!(const_fold(op::SUB, &[U256::from(10), U256::from(3)]), Some(U256::from(7)));
        assert_eq!(const_fold(op::SUB, &[U256::ZERO, U256::from(1)]), Some(U256::MAX));
    }

    #[test]
    fn const_fold_div() {
        // DIV(10, 3) = 3.
        assert_eq!(const_fold(op::DIV, &[U256::from(10), U256::from(3)]), Some(U256::from(3)));
        assert_eq!(const_fold(op::DIV, &[U256::from(7), U256::ZERO]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_sdiv() {
        assert_eq!(const_fold(op::SDIV, &[U256::from(10), U256::from(3)]), Some(U256::from(3)));
        let neg10 = U256::ZERO.wrapping_sub(U256::from(10));
        let neg3 = U256::ZERO.wrapping_sub(U256::from(3));
        // SDIV(-10, 3) = -3.
        assert_eq!(const_fold(op::SDIV, &[neg10, U256::from(3)]), Some(neg3));
        assert_eq!(const_fold(op::SDIV, &[U256::from(7), U256::ZERO]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_mod() {
        // MOD(10, 3) = 1.
        assert_eq!(const_fold(op::MOD, &[U256::from(10), U256::from(3)]), Some(U256::from(1)));
        assert_eq!(const_fold(op::MOD, &[U256::from(7), U256::ZERO]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_smod() {
        let neg8 = U256::ZERO.wrapping_sub(U256::from(8));
        let neg3 = U256::ZERO.wrapping_sub(U256::from(3));
        let neg2 = U256::ZERO.wrapping_sub(U256::from(2));
        // SMOD(-8, 3) = -2.
        assert_eq!(const_fold(op::SMOD, &[neg8, U256::from(3)]), Some(neg2));
        // SMOD(-8, -3) = -2.
        assert_eq!(const_fold(op::SMOD, &[neg8, neg3]), Some(neg2));
    }

    #[test]
    fn const_fold_addmod() {
        // ADDMOD(10, 10, 8) = 4.
        assert_eq!(
            const_fold(op::ADDMOD, &[U256::from(10), U256::from(10), U256::from(8)]),
            Some(U256::from(4)),
        );
        assert_eq!(
            const_fold(op::ADDMOD, &[U256::from(1), U256::from(2), U256::ZERO]),
            Some(U256::ZERO),
        );
    }

    #[test]
    fn const_fold_mulmod() {
        // MULMOD(10, 10, 8) = 4.
        assert_eq!(
            const_fold(op::MULMOD, &[U256::from(10), U256::from(10), U256::from(8)]),
            Some(U256::from(4)),
        );
    }

    #[test]
    fn const_fold_exp() {
        // EXP(2, 10) = 1024.
        assert_eq!(const_fold(op::EXP, &[U256::from(2), U256::from(10)]), Some(U256::from(1024)));
        // EXP(5, 0) = 1.
        assert_eq!(const_fold(op::EXP, &[U256::from(5), U256::ZERO]), Some(U256::from(1)));
    }

    #[test]
    fn const_fold_signextend() {
        // SIGNEXTEND(0, 0xFF) -> all ones.
        assert_eq!(const_fold(op::SIGNEXTEND, &[U256::ZERO, U256::from(0xFF)]), Some(U256::MAX));
        // SIGNEXTEND(0, 0x7F) -> 0x7F.
        assert_eq!(
            const_fold(op::SIGNEXTEND, &[U256::ZERO, U256::from(0x7F)]),
            Some(U256::from(0x7F)),
        );
        // ext >= 31 -> no-op.
        assert_eq!(
            const_fold(op::SIGNEXTEND, &[U256::from(31), U256::from(0xFF)]),
            Some(U256::from(0xFF)),
        );
    }

    #[test]
    fn const_fold_lt_gt_eq() {
        // LT(1, 2) = 1.
        assert_eq!(const_fold(op::LT, &[U256::from(1), U256::from(2)]), Some(U256::from(1)));
        assert_eq!(const_fold(op::LT, &[U256::from(2), U256::from(1)]), Some(U256::ZERO));
        // GT(2, 1) = 1.
        assert_eq!(const_fold(op::GT, &[U256::from(2), U256::from(1)]), Some(U256::from(1)));
        assert_eq!(const_fold(op::GT, &[U256::from(1), U256::from(2)]), Some(U256::ZERO));
        assert_eq!(const_fold(op::EQ, &[U256::from(5), U256::from(5)]), Some(U256::from(1)));
        assert_eq!(const_fold(op::EQ, &[U256::from(5), U256::from(6)]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_slt_sgt() {
        let neg1 = U256::MAX;
        // SLT(-1, 1) = 1.
        assert_eq!(const_fold(op::SLT, &[neg1, U256::from(1)]), Some(U256::from(1)));
        // SLT(1, -1) = 0.
        assert_eq!(const_fold(op::SLT, &[U256::from(1), neg1]), Some(U256::ZERO));
        // SGT(1, -1) = 1.
        assert_eq!(const_fold(op::SGT, &[U256::from(1), neg1]), Some(U256::from(1)));
    }

    #[test]
    fn const_fold_and_or_xor() {
        assert_eq!(
            const_fold(op::AND, &[U256::from(0xFF), U256::from(0x0F)]),
            Some(U256::from(0x0F)),
        );
        assert_eq!(
            const_fold(op::OR, &[U256::from(0xF0), U256::from(0x0F)]),
            Some(U256::from(0xFF)),
        );
        assert_eq!(const_fold(op::XOR, &[U256::from(0xFF), U256::from(0xFF)]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_byte() {
        // BYTE(31, 0xFF) = 0xFF (last byte).
        assert_eq!(
            const_fold(op::BYTE, &[U256::from(31), U256::from(0xFF)]),
            Some(U256::from(0xFF)),
        );
        // BYTE(0, 0xFF) = 0 (0xFF is in byte 31 only).
        assert_eq!(const_fold(op::BYTE, &[U256::ZERO, U256::from(0xFF)]), Some(U256::ZERO));
        // Out of bounds.
        assert_eq!(const_fold(op::BYTE, &[U256::from(32), U256::from(0xFF)]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_shl_shr() {
        // SHL(1, 0x80) = 0x100.
        assert_eq!(
            const_fold(op::SHL, &[U256::from(1), U256::from(0x80)]),
            Some(U256::from(0x100)),
        );
        assert_eq!(const_fold(op::SHL, &[U256::from(256), U256::from(1)]), Some(U256::ZERO));
        // SHR(1, 0x80) = 0x40.
        assert_eq!(const_fold(op::SHR, &[U256::from(1), U256::from(0x80)]), Some(U256::from(0x40)),);
        assert_eq!(const_fold(op::SHR, &[U256::from(256), U256::from(1)]), Some(U256::ZERO));
    }

    #[test]
    fn const_fold_sar() {
        // SAR(1, 2) = 1.
        assert_eq!(const_fold(op::SAR, &[U256::from(1), U256::from(2)]), Some(U256::from(1)));
        // SAR(4, -1) = -1.
        assert_eq!(const_fold(op::SAR, &[U256::from(4), U256::MAX]), Some(U256::MAX));
        // SAR(256, -1) = -1.
        assert_eq!(const_fold(op::SAR, &[U256::from(256), U256::MAX]), Some(U256::MAX));
        // SAR(256, 1) = 0.
        assert_eq!(const_fold(op::SAR, &[U256::from(256), U256::from(1)]), Some(U256::ZERO));
    }

    /// Builds bytecode for N repetitions of `PUSH U256::MAX, PUSH U256::MAX, EXP, POP`.
    fn build_exp_bomb(n: usize) -> Vec<u8> {
        let mut src = String::new();
        for _ in 0..n {
            writeln!(src, "PUSH {}", U256::MAX).unwrap();
            writeln!(src, "PUSH {}", U256::MAX).unwrap();
            writeln!(src, "EXP").unwrap();
            writeln!(src, "POP").unwrap();
        }
        writeln!(src, "STOP").unwrap();
        crate::parse_asm(&src).unwrap()
    }

    /// Adversarial input: many EXP(U256::MAX, U256::MAX) operations.
    /// The gas limit must prevent the compiler from spending unbounded time on these.
    #[test]
    fn compiler_gas_limit_exp_bomb() {
        crate::tests::init_tracing();

        let code = build_exp_bomb(500);
        let start = Instant::now();
        let mut bytecode = Bytecode::test(code);
        bytecode.analyze().unwrap();
        let elapsed = start.elapsed();

        // With the default 100k gas budget, EXP(U256::MAX, U256::MAX) costs 10+50*32=1610 gas
        // each, so we can fold ~62 before hitting the limit. The remaining 438 stay dynamic,
        // but the point is the wall-clock bound.
        assert!(
            elapsed.as_secs() < 30,
            "compilation took too long ({elapsed:?}), gas limit may not be working",
        );
        assert!(bytecode.compiler_gas_used <= bytecode.compiler_gas_limit);
    }

    /// Proves that without a gas limit, high-volume folding is measurably slower.
    ///
    /// With the default 100k budget the same input finishes much faster because most folds
    /// are skipped after the budget runs out.
    #[test]
    fn compiler_gas_limit_no_limit_is_slower() {
        crate::tests::init_tracing();

        // 100k cheap ADD folds — enough to show a measurable difference.
        let mut src = String::new();
        for _ in 0..100_000 {
            writeln!(src, "PUSH 0x01").unwrap();
            writeln!(src, "PUSH 0x02").unwrap();
            writeln!(src, "ADD").unwrap();
            writeln!(src, "POP").unwrap();
        }
        writeln!(src, "STOP").unwrap();
        let code = crate::parse_asm(&src).unwrap();

        // With default (100k) gas limit.
        let start_limited = Instant::now();
        let mut limited = Bytecode::test(code.clone());
        limited.analyze().unwrap();
        let elapsed_limited = start_limited.elapsed();

        // With unlimited gas.
        let start_unlimited = Instant::now();
        let mut unlimited = Bytecode::test(code);
        unlimited.compiler_gas_limit = u64::MAX;
        unlimited.analyze().unwrap();
        let elapsed_unlimited = start_unlimited.elapsed();

        eprintln!("limited  (100k): {elapsed_limited:?} (gas used: {})", limited.compiler_gas_used);
        eprintln!(
            "unlimited (MAX): {elapsed_unlimited:?} (gas used: {})",
            unlimited.compiler_gas_used,
        );

        // The unlimited run folds everything so it must use more gas.
        assert!(unlimited.compiler_gas_used > limited.compiler_gas_used);
        // And the limited run should have hit the cap.
        assert!(limited.compiler_gas_used <= limited.compiler_gas_limit);
    }

    /// Adversarial input: thousands of cheap EXP to exhaust gas via volume.
    #[test]
    fn compiler_gas_limit_cheap_exp_volume() {
        crate::tests::init_tracing();

        // 20K repetitions of EXP(base, small_exponent) — cheap per-op but high volume.
        let mut src = String::new();
        for _ in 0..20_000 {
            writeln!(src, "PUSH {}", U256::MAX).unwrap();
            writeln!(src, "PUSH 0xff").unwrap(); // 1 byte exponent = 60 gas
            writeln!(src, "EXP").unwrap();
            writeln!(src, "POP").unwrap();
        }
        writeln!(src, "STOP").unwrap();

        let code = crate::parse_asm(&src).unwrap();
        let start = Instant::now();
        let mut bytecode = Bytecode::test(code);
        bytecode.analyze().unwrap();
        let elapsed = start.elapsed();

        assert!(elapsed.as_secs() < 30, "compilation took too long ({elapsed:?})",);
        assert!(bytecode.compiler_gas_used <= bytecode.compiler_gas_limit);
    }

    /// Verify that setting compiler_gas_limit to 0 disables constant folding entirely.
    #[test]
    fn compiler_gas_limit_zero_disables_folding() {
        crate::tests::init_tracing();

        let src = "PUSH 2\nPUSH 3\nADD\nPUSH0\nMSTORE\nSTOP\n";
        let code = crate::parse_asm(src).unwrap();
        let mut bytecode = Bytecode::test(code);
        bytecode.compiler_gas_limit = 0;
        bytecode.analyze().unwrap();

        // With gas limit 0, no folding should occur.
        assert_eq!(bytecode.compiler_gas_used, 0);
        // inst layout: PUSH(0), PUSH(1), ADD(2), PUSH0(3), MSTORE(4), STOP(5).
        // The ADD result should NOT be folded — operand 1 at MSTORE should be None.
        assert!(bytecode.const_operand(Inst::from_usize(4), 1).is_none());
    }

    /// Verify that a very large gas limit allows all folding to proceed.
    #[test]
    fn compiler_gas_limit_unlimited() {
        crate::tests::init_tracing();

        let src = "PUSH 2\nPUSH 3\nADD\nPUSH0\nMSTORE\nSTOP\n";
        let code = crate::parse_asm(src).unwrap();
        let mut bytecode = Bytecode::test(code);
        bytecode.compiler_gas_limit = u64::MAX;
        bytecode.analyze().unwrap();

        assert!(bytecode.compiler_gas_used > 0);
        // inst layout: PUSH(0), PUSH(1), ADD(2), PUSH0(3), MSTORE(4), STOP(5).
        assert_eq!(bytecode.const_operand(Inst::from_usize(4), 1), Some(U256::from(5)));
    }

    /// Exhaustively verify that `const_fold_gas` and `try_const_fold` agree on which opcodes
    /// they handle. For every opcode, if `try_const_fold` returns `Some` then `const_fold_gas`
    /// must also return `Some`, and vice versa.
    #[test]
    fn const_fold_gas_sync() {
        let mut interner: crate::bytecode::U256Interner = crate::bytecode::Interner::new();
        let one = U256Imm::new(U256::from(1), &mut interner);
        let two = U256Imm::new(U256::from(2), &mut interner);
        let three = U256Imm::new(U256::from(3), &mut interner);
        let c1 = AbsValue::Const(one);
        let c2 = AbsValue::Const(two);
        let c3 = AbsValue::Const(three);

        for opcode in 0..=u8::MAX {
            let inst = crate::InstData { opcode, ..crate::InstData::new(opcode) };
            let (inp, _) = inst.stack_io();

            let inputs: &[AbsValue] = match inp {
                0 => &[],
                1 => &[c1],
                2 => &[c1, c2],
                3 => &[c1, c2, c3],
                _ => continue,
            };

            let gas = const_fold_gas(opcode, inputs, &interner);
            let fold = try_const_fold(&inst, inputs, &mut interner, 100);

            assert_eq!(
                gas.is_some(),
                fold.is_some(),
                "const_fold_gas and try_const_fold disagree on opcode {opcode:#04x} ({}): \
                 gas={gas:?}, fold={fold:?}",
                revm_bytecode::opcode::OpCode::new(opcode).map_or("unknown", |o| o.as_str()),
            );
        }
    }
}
