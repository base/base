//! ⚠️ Collection of bignum algorithms.
//!
//! <div class="warning">
//! Functions in this module are currently not considered part of the stable API
//! and may be changed or removed in future minor releases, without prior
//! notice.
//! </div>

#![allow(missing_docs)]

macro_rules! unstable_warning {
    () => {
        "\n\n<div class=\"warning\">⚠️ This function is not part of the stable API.</div>\n\n"
    };
}
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::_subborrow_u64;
use core::cmp::Ordering;

pub(crate) use unstable_warning;

mod add;
pub mod div;
mod gcd;
mod mul;
mod mul_redc;

pub use self::{
    add::{borrowing_sub, borrowing_sub_n, carrying_add, carrying_add_n},
    div::div,
    gcd::{LehmerMatrix, gcd, gcd_extended, inv_mod},
    mul::{add_nx1, addmul, addmul_n, addmul_nx1, mul_nx1, submul_nx1},
    mul_redc::{mul_redc, square_redc},
};

pub(crate) struct DW;

#[allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
impl DW {
    /// `high << 64 | low`
    #[inline(always)]
    pub(crate) const fn join(high: u64, low: u64) -> u128 {
        ((high as u128) << 64) | low as u128
    }

    /// `(low, high)`
    #[inline(always)]
    pub(crate) const fn split(double: u128) -> (u64, u64) {
        (double as u64, (double >> 64) as u64)
    }

    /// `a * b + c + d`
    #[inline(always)]
    pub(crate) const fn muladd2(a: u64, b: u64, c: u64, d: u64) -> u128 {
        #[cfg(feature = "nightly")]
        {
            let (low, high) = u64::carrying_mul_add(a, b, c, d);
            Self::join(high, low)
        }
        #[cfg(not(feature = "nightly"))]
        {
            (a as u128) * (b as u128) + (c as u128) + (d as u128)
        }
    }

    /// `a + b`
    #[inline(always)]
    pub(crate) const fn add(a: u64, b: u64) -> u128 {
        Self::muladd2(0, 0, a, b)
    }

    /// `a * b`
    #[inline(always)]
    pub(crate) const fn mul(a: u64, b: u64) -> u128 {
        Self::muladd2(a, b, 0, 0)
    }

    /// `a * b + c`
    #[inline(always)]
    pub(crate) const fn muladd(a: u64, b: u64, c: u64) -> u128 {
        Self::muladd2(a, b, c, 0)
    }

    #[inline(always)]
    pub(crate) const fn high(double: u128) -> u64 {
        Self::split(double).1
    }

    #[inline(always)]
    pub(crate) const fn low(double: u128) -> u64 {
        Self::split(double).0
    }
}

/// ⚠️ Compare two limb slices in reverse order.
#[doc = crate::algorithms::unstable_warning!()]
/// Assumes that if the slices are of different length, the longer slice is
/// always greater than the shorter slice.
#[inline(always)]
#[must_use]
pub fn cmp(a: &[u64], b: &[u64]) -> Ordering {
    match a.len().cmp(&b.len()) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }
    let (r, o) = sub(a, b);
    if r == 0 {
        Ordering::Equal
    } else if o {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

macro_rules! cmp_fns {
    ($($name:ident, $op:literal => |$a:ident, $b:ident| $impl:expr),* $(,)?) => {
        $(
            /// Compare two limb slices in reverse order, returns `true` if
            #[doc = concat!("`a ", $op, " b`.")]
            ///
            /// Assumes that if the slices are of different length, the longer slice is
            /// always greater than the shorter slice.
            #[inline(always)]
            #[must_use]
            pub fn $name($a: &[u64], $b: &[u64]) -> bool {
                $impl
            }
        )*
    };
}

cmp_fns! {
    lt, "<"  => |a, b| match a.len().cmp(&b.len()) {
        Ordering::Equal => sub(a, b).1,
        non_eq => non_eq.is_lt(),
    },
    gt, ">"  => |a, b| lt(b, a),
    ge, ">=" => |a, b| !lt(a, b),
    le, "<=" => |a, b| !lt(b, a),
}

/// `a - b`, returns `((a - b).fold(0, bit_or), overflow)`.
#[inline]
fn sub(a: &[u64], b: &[u64]) -> (u64, bool) {
    assume!(a.len() == b.len());

    #[cfg(target_arch = "x86_64")]
    {
        sub_x86_64(a, b)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        sub_fallback(a, b)
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn sub_x86_64(a: &[u64], b: &[u64]) -> (u64, bool) {
    let mut borrow = 0;
    let mut acc = 0;
    for i in 0..a.len() {
        let mut x = 0;
        // SAFETY: `_subborrow_u64` has no target features beyond x86-64.
        #[allow(unused_unsafe)]
        unsafe {
            borrow = _subborrow_u64(borrow, a[i], b[i], &mut x);
        }
        acc |= x;
    }
    (acc, borrow != 0)
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
fn sub_fallback(a: &[u64], b: &[u64]) -> (u64, bool) {
    let mut borrow = false;
    let mut acc = 0;
    for i in 0..a.len() {
        let x;
        (x, borrow) = borrowing_sub(a[i], b[i], borrow);
        acc |= x;
    }
    // HACK: This only black-boxes `acc` so LLVM keeps the subtraction results
    // alive long enough for the backend to recognize the `borrowing_sub` chain
    // and generate better code: https://github.com/rust-lang/rust/issues/143517
    // SAFETY: Writing to a local variable through a reference is safe.
    unsafe { core::ptr::write_volatile(&mut acc, acc) };
    (acc, borrow)
}

#[inline]
pub(crate) const fn trim_end_zeros(mut x: &[u64]) -> &[u64] {
    while let [rest @ .., 0] = x {
        x = rest;
    }
    x
}

#[inline]
pub(crate) fn trim_end_zeros_mut(mut x: &mut [u64]) -> &mut [u64] {
    while let [rest @ .., 0] = x {
        x = rest;
    }
    x
}
