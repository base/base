use evm2_macros::instruction;

use crate::interpreter::{InstrStop, Word};

#[instruction]
pub(crate) fn pop([_value]: [Word]) -> Result {}

#[instruction(no_stack_preamble)]
pub(crate) fn push<const N: usize>(cx: _) -> Result {
    if N == 0 {
        return stack.push(Word::ZERO);
    }
    let slice = unsafe { cx.pc.read_bytes_offset_unchecked(1, N) };
    stack.push_slice(slice)
}

#[instruction(no_stack_preamble)]
pub(crate) fn dup<const N: usize>() -> Result {
    stack.dup(N)
}

#[instruction(no_stack_preamble)]
pub(crate) fn swap<const N: usize>() -> Result {
    stack.swap(N)
}

#[instruction(no_stack_preamble)]
pub(crate) fn dupn(cx: _) -> Result {
    let n = decode_single(unsafe { cx.pc.read_bytes_offset_unchecked(1, 1)[0] })
        .ok_or(InstrStop::InvalidImmediateEncoding)?;
    stack.dup(n)
}

#[instruction(no_stack_preamble)]
pub(crate) fn swapn(cx: _) -> Result {
    let n = decode_single(unsafe { cx.pc.read_bytes_offset_unchecked(1, 1)[0] })
        .ok_or(InstrStop::InvalidImmediateEncoding)?;
    stack.exchange(0, n)
}

#[instruction(no_stack_preamble)]
pub(crate) fn exchange(cx: _) -> Result {
    let (n, m) = decode_pair(unsafe { cx.pc.read_bytes_offset_unchecked(1, 1)[0] })
        .ok_or(InstrStop::InvalidImmediateEncoding)?;
    stack.exchange(n, m)
}

const fn decode_single(x: u8) -> Option<usize> {
    if x <= 90 || x >= 128 { Some(x.wrapping_add(145) as usize) } else { None }
}

const fn decode_pair(x: u8) -> Option<(usize, usize)> {
    if x > 81 && x < 128 {
        return None;
    }
    let k = (x ^ 143) as usize;
    let q = k / 16;
    let r = k % 16;
    if q < r { Some((q + 1, r + 1)) } else { Some((r + 1, 29 - q)) }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::assert_matches;

    use crate::{
        SpecId,
        interpreter::{InstrStop, StackMut, Word, op},
        test_utils::{RunConfig, push, run, run_stack},
    };

    #[test]
    fn pop_opcode() {
        let interp = run_stack([1], op::POP);
        assert_matches!(interp.err, InstrStop::Stop);
        assert!(interp.stack().is_empty());

        let interp = run(RunConfig::new([op::PUSH1, 0x01, op::PUSH1, 0x02, op::POP, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(1)]);
    }

    #[test]
    fn stack_underflow() {
        let interp = run(RunConfig::new([op::POP]));
        assert_matches!(interp.err, InstrStop::StackUnderflow);

        let interp = run(RunConfig::new([op::DUP1]));
        assert_matches!(interp.err, InstrStop::StackUnderflow);

        let interp = run(RunConfig::new([op::PUSH0, op::SWAP1]));
        assert_matches!(interp.err, InstrStop::StackUnderflow);

        let interp = run(RunConfig::new([op::DUPN, 0x80]).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::StackUnderflow);

        let interp = run(RunConfig::new([op::PUSH0, op::SWAPN, 0x80]).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::StackUnderflow);

        let interp = run(RunConfig::new([op::PUSH0, op::EXCHANGE, 0x8e]).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::StackUnderflow);
    }

    #[test]
    fn stack_overflow() {
        let mut code = vec![op::PUSH0; StackMut::CAPACITY];
        let interp = run(RunConfig::new(code.clone()));
        assert_matches!(interp.err, InstrStop::Stop);
        code.extend([op::PUSH0]);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::StackOverflow);

        let mut code = vec![op::PUSH0; StackMut::CAPACITY];
        code.extend([op::PUSH1, 0x00, op::STOP]);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::StackOverflow);

        let mut code = vec![op::PUSH0; StackMut::CAPACITY];
        code.extend([op::DUP1, op::STOP]);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::StackOverflow);
    }

    #[test]
    fn push0_opcode() {
        let interp = run(RunConfig::new([op::PUSH0, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);

        let interp = run(RunConfig::new([op::PUSH0, op::PUSH0, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0, 0]);
    }

    fn assert_push_opcode(opcode: u8, n: usize) {
        for bytes in [vec![0; n], vec![n as u8; n], (1..=n as u8).collect::<Vec<_>>()] {
            let mut code = Vec::new();
            code.push(opcode);
            code.extend_from_slice(&bytes);
            code.push(op::STOP);

            let interp = run(RunConfig::new(code));
            assert_matches!(interp.err, InstrStop::Stop);
            assert_eq!(interp.stack(), [Word::from_be_slice(&bytes)]);
        }
    }

    macro_rules! push_tests {
        ($($name:ident, $opcode:expr, $n:expr;)*) => {
            $(
                #[test]
                fn $name() {
                    assert_push_opcode($opcode, $n);
                }
            )*
        }
    }

    push_tests! {
        push1_opcode, op::PUSH1, 1;
        push2_opcode, op::PUSH2, 2;
        push3_opcode, op::PUSH3, 3;
        push4_opcode, op::PUSH4, 4;
        push5_opcode, op::PUSH5, 5;
        push6_opcode, op::PUSH6, 6;
        push7_opcode, op::PUSH7, 7;
        push8_opcode, op::PUSH8, 8;
        push9_opcode, op::PUSH9, 9;
        push10_opcode, op::PUSH10, 10;
        push11_opcode, op::PUSH11, 11;
        push12_opcode, op::PUSH12, 12;
        push13_opcode, op::PUSH13, 13;
        push14_opcode, op::PUSH14, 14;
        push15_opcode, op::PUSH15, 15;
        push16_opcode, op::PUSH16, 16;
        push17_opcode, op::PUSH17, 17;
        push18_opcode, op::PUSH18, 18;
        push19_opcode, op::PUSH19, 19;
        push20_opcode, op::PUSH20, 20;
        push21_opcode, op::PUSH21, 21;
        push22_opcode, op::PUSH22, 22;
        push23_opcode, op::PUSH23, 23;
        push24_opcode, op::PUSH24, 24;
        push25_opcode, op::PUSH25, 25;
        push26_opcode, op::PUSH26, 26;
        push27_opcode, op::PUSH27, 27;
        push28_opcode, op::PUSH28, 28;
        push29_opcode, op::PUSH29, 29;
        push30_opcode, op::PUSH30, 30;
        push31_opcode, op::PUSH31, 31;
        push32_opcode, op::PUSH32, 32;
    }

    fn assert_dup_opcode(opcode: u8, n: usize) {
        for offset in [0, 100, 200] {
            let mut code = Vec::new();
            for value in 1..=16 {
                push(&mut code, Word::from(value + offset));
            }
            code.push(opcode);
            code.push(op::STOP);

            let interp = run(RunConfig::new(code));
            assert_matches!(interp.err, InstrStop::Stop);
            assert_eq!(interp.stack().len(), 17);
            assert_eq!(interp.stack()[16], Word::from(17 - n + offset));
        }
    }

    macro_rules! dup_tests {
        ($($name:ident, $opcode:expr, $n:expr;)*) => {
            $(
                #[test]
                fn $name() {
                    assert_dup_opcode($opcode, $n);
                }
            )*
        }
    }

    dup_tests! {
        dup1_opcode, op::DUP1, 1;
        dup2_opcode, op::DUP2, 2;
        dup3_opcode, op::DUP3, 3;
        dup4_opcode, op::DUP4, 4;
        dup5_opcode, op::DUP5, 5;
        dup6_opcode, op::DUP6, 6;
        dup7_opcode, op::DUP7, 7;
        dup8_opcode, op::DUP8, 8;
        dup9_opcode, op::DUP9, 9;
        dup10_opcode, op::DUP10, 10;
        dup11_opcode, op::DUP11, 11;
        dup12_opcode, op::DUP12, 12;
        dup13_opcode, op::DUP13, 13;
        dup14_opcode, op::DUP14, 14;
        dup15_opcode, op::DUP15, 15;
        dup16_opcode, op::DUP16, 16;
    }

    fn assert_swap_opcode(opcode: u8, n: usize) {
        for offset in [0, 100, 200] {
            let mut code = Vec::new();
            for value in 1..=17 {
                push(&mut code, Word::from(value + offset));
            }
            code.push(opcode);
            code.push(op::STOP);

            let interp = run(RunConfig::new(code));
            assert_matches!(interp.err, InstrStop::Stop);
            assert_eq!(interp.stack().len(), 17);
            assert_eq!(interp.stack()[16], Word::from(17 - n + offset));
            assert_eq!(interp.stack()[16 - n], Word::from(17 + offset));
        }
    }

    macro_rules! swap_tests {
        ($($name:ident, $opcode:expr, $n:expr;)*) => {
            $(
                #[test]
                fn $name() {
                    assert_swap_opcode($opcode, $n);
                }
            )*
        }
    }

    swap_tests! {
        swap1_opcode, op::SWAP1, 1;
        swap2_opcode, op::SWAP2, 2;
        swap3_opcode, op::SWAP3, 3;
        swap4_opcode, op::SWAP4, 4;
        swap5_opcode, op::SWAP5, 5;
        swap6_opcode, op::SWAP6, 6;
        swap7_opcode, op::SWAP7, 7;
        swap8_opcode, op::SWAP8, 8;
        swap9_opcode, op::SWAP9, 9;
        swap10_opcode, op::SWAP10, 10;
        swap11_opcode, op::SWAP11, 11;
        swap12_opcode, op::SWAP12, 12;
        swap13_opcode, op::SWAP13, 13;
        swap14_opcode, op::SWAP14, 14;
        swap15_opcode, op::SWAP15, 15;
        swap16_opcode, op::SWAP16, 16;
    }

    #[test]
    fn dupn_opcode() {
        let mut code = vec![op::PUSH1, 0x01, op::PUSH1, 0x00];
        code.extend(core::iter::repeat_n(op::DUP1, 15));
        code.extend([op::DUPN, 0x80, op::STOP]);
        let interp = run(RunConfig::new(code).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack().len(), 18);
        assert_eq!(interp.stack()[17], Word::from(1));
        assert_eq!(interp.stack()[0], Word::from(1));
        for i in 1..17 {
            assert_eq!(interp.stack()[i], 0);
        }

        let mut code = Vec::new();
        for value in 0..145 {
            push(&mut code, Word::from(value));
        }
        code.extend([op::DUPN, 0xff, op::STOP]);
        let interp = run(RunConfig::new(code).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack().len(), 146);
        assert_eq!(interp.stack()[145], Word::from(1));
    }

    #[test]
    fn swapn_opcode() {
        let mut code = vec![op::PUSH1, 0x01, op::PUSH1, 0x00];
        code.extend(core::iter::repeat_n(op::DUP1, 15));
        code.extend([op::PUSH1, 0x02, op::SWAPN, 0x80, op::STOP]);
        let interp = run(RunConfig::new(code).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack().len(), 18);
        assert_eq!(interp.stack()[17], Word::from(1));
        assert_eq!(interp.stack()[0], Word::from(2));
        for i in 1..17 {
            assert_eq!(interp.stack()[i], 0);
        }

        let mut code = Vec::new();
        for value in 0..145 {
            push(&mut code, Word::from(value));
        }
        code.extend([op::SWAPN, 0xff, op::STOP]);
        let interp = run(RunConfig::new(code).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack()[0], Word::from(144));
        assert_eq!(interp.stack()[144], 0);
    }

    #[test]
    fn exchange_opcode() {
        let interp = run(RunConfig::new([
            op::PUSH1,
            0x00,
            op::PUSH1,
            0x01,
            op::PUSH1,
            0x02,
            op::EXCHANGE,
            0x8e,
            op::STOP,
        ])
        .spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(1), Word::from(0), Word::from(2)]);

        let mut code = Vec::new();
        for value in 0..23 {
            push(&mut code, Word::from(value));
        }
        code.extend([op::EXCHANGE, 0xff, op::STOP]);
        let interp = run(RunConfig::new(code).spec(SpecId::AMSTERDAM));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack()[0], Word::from(21));
        assert_eq!(interp.stack()[21], 0);
        assert_eq!(interp.stack()[22], Word::from(22));
    }

    #[test]
    fn relative_stack_opcodes_are_not_enabled_before_amsterdam() {
        let interp = run(RunConfig::new([op::DUPN, 0x80]).spec(SpecId::OSAKA));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);

        let interp = run(RunConfig::new([op::SWAPN, 0x80]).spec(SpecId::OSAKA));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);

        let interp = run(RunConfig::new([op::EXCHANGE, 0x8e]).spec(SpecId::OSAKA));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
    }
}
