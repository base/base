#[cfg(feature = "alloc")]
#[allow(unused_imports)]
use alloc::vec::Vec;

/// A wrapper around a slice that panics on out-of-bounds indexing only in debug
/// mode.
///
/// This is a better default for this crate since it is critical for performance
/// to remove these checks in optimized builds.
#[repr(transparent)]
pub(crate) struct UncheckedSlice<T>([T]);

impl<T> UncheckedSlice<T> {
    #[inline]
    pub(crate) const fn wrap(slice: &[T]) -> &Self {
        // SAFETY: `#[repr(transparent)]`.
        unsafe { &*(slice as *const [T] as *const Self) }
    }

    #[inline]
    pub(crate) fn wrap_mut(slice: &mut [T]) -> &mut Self {
        // SAFETY: `#[repr(transparent)]`.
        unsafe { &mut *(slice as *mut [T] as *mut Self) }
    }
}

impl<T> core::ops::Deref for UncheckedSlice<T> {
    type Target = [T];

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> core::ops::DerefMut for UncheckedSlice<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T, I: core::slice::SliceIndex<[T]>> core::ops::Index<I> for UncheckedSlice<T> {
    type Output = <I as core::slice::SliceIndex<[T]>>::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        #[cfg(debug_assertions)]
        return self.0.index(index);
        #[cfg(not(debug_assertions))]
        return unsafe { self.0.get_unchecked(index) };
    }
}
impl<T, I: core::slice::SliceIndex<[T]>> core::ops::IndexMut<I> for UncheckedSlice<T> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        #[cfg(debug_assertions)]
        return self.0.index_mut(index);
        #[cfg(not(debug_assertions))]
        return unsafe { self.0.get_unchecked_mut(index) };
    }
}

/// Like `a % b` but returns `b` instead of `0`.
#[allow(dead_code)] // This is used by some support features.
#[must_use]
pub(crate) const fn rem_up(a: usize, b: usize) -> usize {
    let rem = a % b;
    if rem > 0 { rem } else { b }
}

#[allow(dead_code)] // This is used by some support features.
#[inline]
fn last_idx<T: PartialEq>(x: &[T], value: &T) -> usize {
    x.iter().rposition(|b| b != value).map_or(0, |idx| idx + 1)
}

#[allow(dead_code)] // This is used by some support features.
#[inline]
#[must_use]
pub(crate) fn trim_end_slice<'a, T: PartialEq>(slice: &'a [T], value: &T) -> &'a [T] {
    &slice[..last_idx(slice, value)]
}

#[cfg(feature = "alloc")]
#[inline]
pub(crate) fn trim_end_vec<T: PartialEq>(vec: &mut Vec<T>, value: &T) {
    vec.truncate(last_idx(vec, value));
}

/// Returns the highest power of `n` that fits in `u64`.
#[inline]
pub(crate) const fn max_pow_u64(n: u64) -> u64 {
    match n {
        2 | 8 => 1 << 63,
        10 => 10_000_000_000_000_000_000,
        16 => 1 << 60,
        _ => max_pow_u64_impl(n),
    }
}

#[inline]
const fn max_pow_u64_impl(n: u64) -> u64 {
    let mut max = n;
    while let Some(next) = max.checked_mul(n) {
        max = next;
    }
    max
}

// Branch prediction hints.
#[cfg(feature = "nightly")]
pub(crate) use core::intrinsics::{cold_path, likely, select_unpredictable, unlikely};

#[cfg(not(feature = "nightly"))]
#[inline(always)]
#[cold]
pub(crate) const fn cold_path() {}

#[cfg(not(feature = "nightly"))]
#[inline(always)]
pub(crate) fn likely(b: bool) -> bool {
    if b {
        true
    } else {
        cold_path();
        false
    }
}

#[cfg(not(feature = "nightly"))]
#[inline(always)]
pub(crate) fn unlikely(b: bool) -> bool {
    if b {
        cold_path();
        true
    } else {
        false
    }
}

#[cfg(not(feature = "nightly"))]
#[inline(always)]
pub(crate) fn select_unpredictable<T>(b: bool, true_val: T, false_val: T) -> T {
    if b { true_val } else { false_val }
}

#[inline(always)]
pub(crate) const fn select_unpredictable_u32(b: bool, true_val: u32, false_val: u32) -> u32 {
    if b { true_val } else { false_val }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trim() {
        assert_eq!(trim_end_slice(&[], &0), &[] as &[i32]);
        assert_eq!(trim_end_slice(&[0], &0), &[] as &[i32]);
        assert_eq!(trim_end_slice(&[0, 1], &0), &[0, 1]);
        assert_eq!(trim_end_slice(&[0, 1, 0], &0), &[0, 1]);
        assert_eq!(trim_end_slice(&[0, 1, 0, 0], &0), &[0, 1]);
        assert_eq!(trim_end_slice(&[0, 1, 0, 0, 0], &0), &[0, 1]);
        assert_eq!(trim_end_slice(&[0, 1, 0, 1, 0], &0), &[0, 1, 0, 1]);

        let trim_end_vec = |mut v: Vec<i32>, x: &i32| {
            trim_end_vec(&mut v, x);
            v
        };
        assert_eq!(trim_end_vec(vec![], &0), &[] as &[i32]);
        assert_eq!(trim_end_vec(vec![0], &0), &[] as &[i32]);
        assert_eq!(trim_end_vec(vec![0, 1], &0), &[0, 1]);
        assert_eq!(trim_end_vec(vec![0, 1, 0], &0), &[0, 1]);
        assert_eq!(trim_end_vec(vec![0, 1, 0, 0], &0), &[0, 1]);
        assert_eq!(trim_end_vec(vec![0, 1, 0, 0, 0], &0), &[0, 1]);
        assert_eq!(trim_end_vec(vec![0, 1, 0, 1, 0], &0), &[0, 1, 0, 1]);
    }

    #[test]
    fn test_max_pow_u64() {
        for (n, expected) in
            [(2, 1 << 63), (8, 1 << 63), (10, 10_000_000_000_000_000_000), (16, 1 << 60)]
        {
            assert_eq!(max_pow_u64(n), expected);
            assert_eq!(max_pow_u64_impl(n), expected);
        }
    }
}
