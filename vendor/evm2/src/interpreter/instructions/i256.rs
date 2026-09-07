//! Signed 256-bit arithmetic helpers.

use core::cmp::Ordering;

use crate::interpreter::Word;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(dead_code)]
#[repr(i8)]
pub(super) enum Sign {
    Minus = -1,
    Zero = 0,
    Plus = 1,
}

#[allow(dead_code)]
pub(super) const MAX_POSITIVE_VALUE: Word = Word::from_limbs([
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0x7fffffffffffffff,
]);

pub(super) const MIN_NEGATIVE_VALUE: Word = Word::from_limbs([
    0x0000000000000000,
    0x0000000000000000,
    0x0000000000000000,
    0x8000000000000000,
]);

const FLIPH_BITMASK_U64: u64 = 0x7FFF_FFFF_FFFF_FFFF;

#[inline]
pub(super) fn i256_sign(val: &Word) -> Sign {
    if val.bit(Word::BITS - 1) {
        Sign::Minus
    } else {
        unsafe { core::mem::transmute::<bool, Sign>(!val.is_zero()) }
    }
}

#[inline]
pub(super) fn i256_sign_compl(val: &mut Word) -> Sign {
    let sign = i256_sign(val);
    if sign == Sign::Minus {
        two_compl_mut(val);
    }
    sign
}

#[inline]
const fn u256_remove_sign(val: &mut Word) {
    unsafe {
        val.as_limbs_mut()[3] &= FLIPH_BITMASK_U64;
    }
}

#[inline]
pub(super) const fn two_compl_mut(op: &mut Word) {
    *op = two_compl(*op);
}

#[inline]
pub(super) const fn two_compl(op: Word) -> Word {
    op.wrapping_neg()
}

/// Compares two EVM words as signed 256-bit integers.
#[inline]
pub fn i256_cmp(first: &Word, second: &Word) -> Ordering {
    let first_sign = i256_sign(first);
    let second_sign = i256_sign(second);
    match first_sign.cmp(&second_sign) {
        Ordering::Equal => first.cmp(second),
        o => o,
    }
}

/// Divides two EVM words as signed 256-bit integers.
#[inline]
pub fn i256_div(mut first: Word, mut second: Word) -> Word {
    let second_sign = i256_sign_compl(&mut second);
    if second_sign == Sign::Zero {
        return Word::ZERO;
    }

    let first_sign = i256_sign_compl(&mut first);
    if first == MIN_NEGATIVE_VALUE && second == Word::from(1) {
        return two_compl(MIN_NEGATIVE_VALUE);
    }

    let mut d = first / second;
    u256_remove_sign(&mut d);

    if (first_sign == Sign::Minus && second_sign != Sign::Minus)
        || (second_sign == Sign::Minus && first_sign != Sign::Minus)
    {
        two_compl(d)
    } else {
        d
    }
}

/// Computes signed 256-bit remainder.
#[inline]
pub fn i256_mod(mut first: Word, mut second: Word) -> Word {
    let first_sign = i256_sign_compl(&mut first);
    if first_sign == Sign::Zero {
        return Word::ZERO;
    }

    let second_sign = i256_sign_compl(&mut second);
    if second_sign == Sign::Zero {
        return Word::ZERO;
    }

    let mut r = first % second;
    u256_remove_sign(&mut r);

    if first_sign == Sign::Minus { two_compl(r) } else { r }
}
