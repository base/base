/// ⚠️ Add with carry.
#[doc = crate::algorithms::unstable_warning!()]
/// Helper while [Rust#85532](https://github.com/rust-lang/rust/issues/85532) stabilizes.
#[inline]
#[must_use]
pub const fn carrying_add(lhs: u64, rhs: u64, carry: bool) -> (u64, bool) {
    #[cfg(feature = "nightly")]
    {
        lhs.carrying_add(rhs, carry)
    }
    #[cfg(not(feature = "nightly"))]
    {
        let (result, carry_1) = lhs.overflowing_add(rhs);
        let (result, carry_2) = result.overflowing_add(carry as u64);
        (result, carry_1 | carry_2)
    }
}

/// ⚠️ Sub with borrow.
#[doc = crate::algorithms::unstable_warning!()]
/// Helper while [Rust#85532](https://github.com/rust-lang/rust/issues/85532) stabilizes.
#[inline]
#[must_use]
pub const fn borrowing_sub(lhs: u64, rhs: u64, borrow: bool) -> (u64, bool) {
    #[cfg(feature = "nightly")]
    {
        lhs.borrowing_sub(rhs, borrow)
    }
    #[cfg(not(feature = "nightly"))]
    {
        let (result, borrow_1) = lhs.overflowing_sub(rhs);
        let (result, borrow_2) = result.overflowing_sub(borrow as u64);
        (result, borrow_1 | borrow_2)
    }
}

/// ⚠️ `lhs += rhs + carry`
#[doc = crate::algorithms::unstable_warning!()]
#[inline(always)]
pub fn carrying_add_n(lhs: &mut [u64], rhs: &[u64], mut carry: bool) -> bool {
    debug_assert!(lhs.len() == rhs.len());
    for i in 0..lhs.len() {
        (lhs[i], carry) = carrying_add(lhs[i], rhs[i], carry);
    }
    carry
}

/// ⚠️ `lhs -= rhs - borrow`
#[doc = crate::algorithms::unstable_warning!()]
#[inline(always)]
pub fn borrowing_sub_n(lhs: &mut [u64], rhs: &[u64], mut borrow: bool) -> bool {
    debug_assert!(lhs.len() == rhs.len());
    for i in 0..lhs.len() {
        (lhs[i], borrow) = borrowing_sub(lhs[i], rhs[i], borrow);
    }
    borrow
}
