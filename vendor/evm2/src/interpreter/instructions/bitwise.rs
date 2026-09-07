use core::cmp::Ordering;

use evm2_macros::instruction;

use super::i256::i256_cmp;
use crate::{interpreter::Word, utils::word_to_usize_saturated};

#[instruction]
pub(crate) fn lt([a, b]: [Word]) -> out {
    *out = Word::from(a < b);
}

#[instruction]
pub(crate) fn gt([a, b]: [Word]) -> out {
    *out = Word::from(a > b);
}

#[instruction]
pub(crate) fn slt([a, b]: [Word]) -> out {
    *out = Word::from(i256_cmp(a, b) == Ordering::Less);
}

#[instruction]
pub(crate) fn sgt([a, b]: [Word]) -> out {
    *out = Word::from(i256_cmp(a, b) == Ordering::Greater);
}

#[instruction]
pub(crate) fn eq([a, b]: [Word]) -> out {
    *out = Word::from(a == b);
}

#[instruction]
pub(crate) fn iszero([value]: [Word]) -> out {
    *out = Word::from(value.is_zero());
}

#[instruction]
pub(crate) fn bitand([a, b]: [Word]) -> out {
    *out = *a & *b;
}

#[instruction]
pub(crate) fn bitor([a, b]: [Word]) -> out {
    *out = *a | *b;
}

#[instruction]
pub(crate) fn bitxor([a, b]: [Word]) -> out {
    *out = *a ^ *b;
}

#[instruction]
pub(crate) fn not([value]: [Word]) -> out {
    *out = !*value;
}

#[instruction]
pub(crate) fn byte([index, value]: [Word]) -> out {
    let index = word_to_usize_saturated(*index);
    *out = if index < 32 { Word::from(value.byte(31 - index)) } else { Word::ZERO };
}

#[instruction]
pub(crate) fn shl([shift, value]: [Word]) -> out {
    let shift = word_to_usize_saturated(*shift);
    *out = if shift < 256 { *value << shift } else { Word::ZERO };
}

#[instruction]
pub(crate) fn shr([shift, value]: [Word]) -> out {
    let shift = word_to_usize_saturated(*shift);
    *out = if shift < 256 { *value >> shift } else { Word::ZERO };
}

#[instruction]
pub(crate) fn sar([shift, value]: [Word]) -> out {
    let shift = word_to_usize_saturated(*shift);
    *out = if shift < 256 {
        value.arithmetic_shr(shift)
    } else if value.bit(255) {
        Word::MAX
    } else {
        Word::ZERO
    };
}

#[instruction]
pub(crate) fn clz([value]: [Word]) -> out {
    *out = Word::from(value.leading_zeros());
}

#[cfg(test)]
mod tests {
    use crate::{
        interpreter::Word,
        test_utils::{assert_stack, neg},
    };

    #[test]
    fn lt_opcode() {
        assert_stack!(LT(1, 2), 1);
        assert_stack!(LT(2, 1), 0);
        assert_stack!(LT(2, 2), 0);
    }

    #[test]
    fn gt_opcode() {
        assert_stack!(GT(2, 1), 1);
        assert_stack!(GT(1, 2), 0);
        assert_stack!(GT(2, 2), 0);
    }

    #[test]
    fn slt_opcode() {
        assert_stack!(SLT(neg(1), 0), 1);
        assert_stack!(SLT(0, neg(1)), 0);
        assert_stack!(SLT(neg(2), neg(1)), 1);
    }

    #[test]
    fn sgt_opcode() {
        assert_stack!(SGT(0, neg(1)), 1);
        assert_stack!(SGT(neg(1), 0), 0);
        assert_stack!(SGT(neg(1), neg(2)), 1);
    }

    #[test]
    fn eq_opcode() {
        assert_stack!(EQ(3, 3), 1);
        assert_stack!(EQ(3, 4), 0);
        assert_stack!(EQ(neg(1), neg(1)), 1);
    }

    #[test]
    fn iszero_opcode() {
        assert_stack!(ISZERO(0), 1);
        assert_stack!(ISZERO(1), 0);
        assert_stack!(ISZERO(neg(1)), 0);
    }

    #[test]
    fn and_opcode() {
        assert_stack!(AND(0b1100, 0b1010), 0b1000);
        assert_stack!(AND(neg(1), 0x55), 0x55);
        assert_stack!(AND(0, neg(1)), 0);
    }

    #[test]
    fn or_opcode() {
        assert_stack!(OR(0b1100, 0b1010), 0b1110);
        assert_stack!(OR(0, 0x55), 0x55);
        assert_stack!(OR(neg(1), 0), neg(1));
    }

    #[test]
    fn xor_opcode() {
        assert_stack!(XOR(0b1100, 0b1010), 0b0110);
        assert_stack!(XOR(neg(1), neg(1)), 0);
        assert_stack!(XOR(0, 0x55), 0x55);
    }

    #[test]
    fn not_opcode() {
        assert_stack!(NOT(0), neg(1));
        assert_stack!(NOT(neg(1)), 0);
        assert_stack!(NOT(0xff), neg(1) - Word::from(0xff));
    }

    #[test]
    fn byte_opcode() {
        assert_stack!(BYTE(31, 0x1234), 0x34);
        assert_stack!(BYTE(30, 0x1234), 0x12);
        assert_stack!(BYTE(32, 0x1234), 0);
    }

    #[test]
    fn shl_opcode() {
        assert_stack!(SHL(8, 1), 256);
        assert_stack!(SHL(0, 7), 7);
        assert_stack!(SHL(256, 1), 0);
    }

    #[test]
    fn shr_opcode() {
        assert_stack!(SHR(8, 256), 1);
        assert_stack!(SHR(0, 7), 7);
        assert_stack!(SHR(256, neg(1)), 0);
    }

    #[test]
    fn sar_opcode() {
        assert_stack!(SAR(1, neg(1) - Word::from(1)), neg(1));
        assert_stack!(SAR(1, 4), 2);
        assert_stack!(SAR(256, neg(1)), neg(1));
    }

    #[test]
    fn clz_opcode() {
        assert_stack!(CLZ(1), 255);
        assert_stack!(CLZ(0), 256);
        assert_stack!(CLZ(neg(1)), 0);
    }
}
