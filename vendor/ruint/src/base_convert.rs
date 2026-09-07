use core::{fmt, iter::FusedIterator, mem::MaybeUninit};

use crate::{
    Uint,
    algorithms::{addmul_nx1, mul_nx1},
};

/// Error for [`from_base_le`][Uint::from_base_le] and
/// [`from_base_be`][Uint::from_base_be].
#[allow(clippy::module_name_repetitions)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseConvertError {
    /// The value is too large to fit the target type.
    Overflow,

    /// The requested number base `.0` is less than two.
    InvalidBase(u64),

    /// The provided digit `.0` is out of range for requested base `.1`.
    InvalidDigit(u64, u64),
}

#[cfg(feature = "std")]
impl std::error::Error for BaseConvertError {}

impl fmt::Display for BaseConvertError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => f.write_str("the value is too large to fit the target type"),
            Self::InvalidBase(base) => {
                write!(f, "the requested number base {base} is less than two")
            }
            Self::InvalidDigit(digit, base) => {
                write!(f, "digit {digit} is out of range for base {base}")
            }
        }
    }
}

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Returns an iterator over the base `base` digits of the number in
    /// little-endian order.
    ///
    /// Pro tip: instead of setting `base = 10`, set it to the highest
    /// power of `10` that still fits `u64`. This way much fewer iterations
    /// are required to extract all the digits.
    // OPT: Internalize this trick so the user won't have to worry about it.
    /// # Panics
    ///
    /// Panics if the base is less than 2.
    #[inline]
    #[track_caller]
    pub fn to_base_le(&self, base: u64) -> impl Iterator<Item = u64> {
        SpigotLittle::new(self.limbs, base)
    }

    /// Returns an iterator over the base `base` digits of the number in
    /// big-endian order.
    ///
    /// Pro tip: instead of setting `base = 10`, set it to the highest
    /// power of `10` that still fits `u64`. This way much fewer iterations
    /// are required to extract all the digits.
    ///
    /// Use [`to_base_be_2`](Self::to_base_be_2) to extract the maximum number
    /// of digits at once more efficiently.
    ///
    /// # Panics
    ///
    /// Panics if the base is less than 2.
    ///
    /// # Examples
    ///
    /// ```
    /// let n = ruint::aliases::U64::from(1234);
    /// assert_eq!(n.to_base_be(10).collect::<Vec<_>>(), [1, 2, 3, 4]);
    /// assert_eq!(n.to_base_be(1000000).collect::<Vec<_>>(), [1234]);
    ///
    /// // `to_base_be_2` always returns digits maximally packed into `u64`s.
    /// assert_eq!(n.to_base_be_2(10).collect::<Vec<_>>(), [1234]);
    /// assert_eq!(n.to_base_be_2(1000000).collect::<Vec<_>>(), [1234]);
    /// ```
    #[inline]
    #[track_caller]
    pub fn to_base_be(&self, base: u64) -> impl Iterator<Item = u64> {
        // Use `to_base_le` if we can heap-allocate it to reverse the order,
        // as it only performs one division per iteration instead of two.
        #[cfg(feature = "alloc")]
        {
            self.to_base_le(base).collect::<alloc::vec::Vec<_>>().into_iter().rev()
        }
        #[cfg(not(feature = "alloc"))]
        {
            SpigotBig::new(*self, base)
        }
    }

    /// Returns an iterator over the base `base` digits of the number in
    /// big-endian order.
    ///
    /// Always returns digits maximally packed into `u64`s.
    /// Unlike [`to_base_be`], this method:
    /// - never heap-allocates memory, so it's always faster
    /// - always returns digits maximally packed into `u64`s, so passing the
    ///   constant base like `2`, `8`, instead of the highest power that fits in
    ///   u64 is not needed
    ///
    /// # Panics
    ///
    /// Panics if the base is less than 2.
    ///
    /// # Examples
    ///
    /// See [`to_base_be`].
    ///
    /// [`to_base_be`]: Self::to_base_be
    #[inline]
    #[track_caller]
    pub fn to_base_be_2(&self, base: u64) -> impl Iterator<Item = u64> {
        SpigotBig2::new(self.limbs, base)
    }

    /// Constructs the [`Uint`] from digits in the base `base` in little-endian.
    ///
    /// # Errors
    ///
    /// * [`BaseConvertError::InvalidBase`] if the base is less than 2.
    /// * [`BaseConvertError::InvalidDigit`] if a digit is out of range.
    /// * [`BaseConvertError::Overflow`] if the number is too large to fit.
    #[inline]
    pub fn from_base_le<I>(base: u64, digits: I) -> Result<Self, BaseConvertError>
    where
        I: IntoIterator<Item = u64>,
    {
        if base < 2 {
            return Err(BaseConvertError::InvalidBase(base));
        }
        if BITS == 0 {
            for digit in digits {
                if digit >= base {
                    return Err(BaseConvertError::InvalidDigit(digit, base));
                }
                if digit != 0 {
                    return Err(BaseConvertError::Overflow);
                }
            }
            return Ok(Self::ZERO);
        }

        let mut iter = digits.into_iter();
        let mut result = Self::ZERO;
        let mut power = Self::ONE;
        for digit in iter.by_ref() {
            if digit >= base {
                return Err(BaseConvertError::InvalidDigit(digit, base));
            }

            // Add digit to result
            let overflow = addmul_nx1(&mut result.limbs, power.as_limbs(), digit);
            if overflow != 0 || result.limbs[LIMBS - 1] > Self::MASK {
                return Err(BaseConvertError::Overflow);
            }

            // Update power
            let overflow = mul_nx1(&mut power.limbs, base);
            if overflow != 0 || power.limbs[LIMBS - 1] > Self::MASK {
                // Following digits must be zero
                break;
            }
        }
        for digit in iter {
            if digit >= base {
                return Err(BaseConvertError::InvalidDigit(digit, base));
            }
            if digit != 0 {
                return Err(BaseConvertError::Overflow);
            }
        }
        Ok(result)
    }

    /// Constructs the [`Uint`] from digits in the base `base` in big-endian.
    ///
    /// # Errors
    ///
    /// * [`BaseConvertError::InvalidBase`] if the base is less than 2.
    /// * [`BaseConvertError::InvalidDigit`] if a digit is out of range.
    /// * [`BaseConvertError::Overflow`] if the number is too large to fit.
    #[inline]
    pub fn from_base_be<I: IntoIterator<Item = u64>>(
        base: u64,
        digits: I,
    ) -> Result<Self, BaseConvertError> {
        // OPT: Special handling of bases that divide 2^64, and bases that are
        // powers of 2.
        // OPT: Same trick as with `to_base_le`, find the largest power of base
        // that fits `u64` and accumulate there first.
        if base < 2 {
            return Err(BaseConvertError::InvalidBase(base));
        }

        let mut result = Self::ZERO;
        for digit in digits {
            if digit >= base {
                return Err(BaseConvertError::InvalidDigit(digit, base));
            }
            // Multiply by base.
            // OPT: keep track of non-zero limbs and mul the minimum.
            let mut carry = u128::from(digit);
            #[allow(clippy::cast_possible_truncation)]
            for limb in &mut result.limbs {
                carry += u128::from(*limb) * u128::from(base);
                *limb = carry as u64;
                carry >>= 64;
            }
            if carry > 0 || (LIMBS != 0 && result.limbs[LIMBS - 1] > Self::MASK) {
                return Err(BaseConvertError::Overflow);
            }
        }

        Ok(result)
    }
}

struct SpigotLittle<const LIMBS: usize> {
    base: u64,
    limbs: [u64; LIMBS],
}

impl<const LIMBS: usize> SpigotLittle<LIMBS> {
    #[inline]
    #[track_caller]
    fn new(limbs: [u64; LIMBS], base: u64) -> Self {
        assert!(base > 1);
        Self { base, limbs }
    }
}

impl<const LIMBS: usize> Iterator for SpigotLittle<LIMBS> {
    type Item = u64;

    #[inline]
    #[allow(clippy::cast_possible_truncation)] // Doesn't truncate.
    fn next(&mut self) -> Option<Self::Item> {
        let base = self.base;
        assume!(base > 1); // Checked in `new`.

        let mut zero = 0_u64;
        let mut remainder = 0_u128;
        for limb in self.limbs.iter_mut().rev() {
            zero |= *limb;
            remainder = (remainder << 64) | u128::from(*limb);
            *limb = (remainder / u128::from(base)) as u64;
            remainder %= u128::from(base);
        }
        if zero == 0 { None } else { Some(remainder as u64) }
    }
}

impl<const LIMBS: usize> FusedIterator for SpigotLittle<LIMBS> {}

/// Implementation of `to_base_be` when `alloc` feature is disabled.
///
/// This is generally slower than simply reversing the result of `to_base_le`
/// as it performs two divisions per iteration instead of one.
#[cfg(not(feature = "alloc"))]
struct SpigotBig<const LIMBS: usize, const BITS: usize> {
    base: u64,
    n: Uint<BITS, LIMBS>,
    power: Uint<BITS, LIMBS>,
    done: bool,
}

#[cfg(not(feature = "alloc"))]
impl<const LIMBS: usize, const BITS: usize> SpigotBig<LIMBS, BITS> {
    #[inline]
    #[track_caller]
    fn new(n: Uint<BITS, LIMBS>, base: u64) -> Self {
        assert!(base > 1);

        Self { n, base, power: Self::highest_power(n, base), done: false }
    }

    /// Returns the largest power of `base` that fits in `n`.
    #[inline]
    fn highest_power(n: Uint<BITS, LIMBS>, base: u64) -> Uint<BITS, LIMBS> {
        let mut power = Uint::ONE;
        if base.is_power_of_two() {
            loop {
                match power.checked_shl(base.trailing_zeros() as _) {
                    Some(p) if p < n => power = p,
                    _ => break,
                }
            }
        } else if let Ok(base) = Uint::try_from(base) {
            loop {
                match power.checked_mul(base) {
                    Some(p) if p < n => power = p,
                    _ => break,
                }
            }
        }
        power
    }
}

#[cfg(not(feature = "alloc"))]
impl<const LIMBS: usize, const BITS: usize> Iterator for SpigotBig<LIMBS, BITS> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let digit;
        if self.power == 1 {
            digit = self.n;
            self.done = true;
        } else if self.base.is_power_of_two() {
            digit = self.n >> self.power.trailing_zeros();
            self.n &= self.power - Uint::ONE;

            self.power >>= self.base.trailing_zeros();
        } else {
            (digit, self.n) = self.n.div_rem(self.power);
            self.power /= Uint::from(self.base);
        }

        match u64::try_from(digit) {
            Ok(digit) => Some(digit),
            Err(e) => debug_unreachable!("digit {digit}: {e}"),
        }
    }
}

#[cfg(not(feature = "alloc"))]
impl<const LIMBS: usize, const BITS: usize> core::iter::FusedIterator for SpigotBig<LIMBS, BITS> {}

/// An iterator over the base `base` digits of the number in big-endian order.
///
/// See [`Uint::to_base_be_2`] for more details.
struct SpigotBig2<const LIMBS: usize> {
    buf: SpigotBuf<LIMBS>,
}

impl<const LIMBS: usize> SpigotBig2<LIMBS> {
    #[inline]
    #[track_caller]
    fn new(limbs: [u64; LIMBS], base: u64) -> Self {
        Self { buf: SpigotBuf::new(limbs, base) }
    }
}

impl<const LIMBS: usize> Iterator for SpigotBig2<LIMBS> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.buf.next_back()
    }
}

impl<const LIMBS: usize> FusedIterator for SpigotBig2<LIMBS> {}

/// Collects [`SpigotLittle`] into a stack-allocated buffer.
///
/// Base for [`SpigotBig2`].
struct SpigotBuf<const LIMBS: usize> {
    end: usize,
    buf: [[MaybeUninit<u64>; 2]; LIMBS],
}

impl<const LIMBS: usize> SpigotBuf<LIMBS> {
    #[inline]
    #[track_caller]
    fn new(limbs: [u64; LIMBS], mut base: u64) -> Self {
        // We need to do this so we can guarantee that `buf` is big enough.
        base = crate::utils::max_pow_u64(base);

        let mut buf = [[MaybeUninit::uninit(); 2]; LIMBS];
        let as_slice = buf.as_flattened_mut();
        let mut i = 0;
        for limb in SpigotLittle::new(limbs, base) {
            debug_assert!(
                i < as_slice.len(),
                "base {base} too small for u64 digits of {LIMBS} limbs; this shouldn't happen \
                 because of the `max_pow_u64` call above"
            );
            unsafe { as_slice.get_unchecked_mut(i).write(limb) };
            i += 1;
        }
        Self { end: i, buf }
    }

    #[inline]
    fn next_back(&mut self) -> Option<u64> {
        if self.end == 0 {
            None
        } else {
            self.end -= 1;
            Some(unsafe { *self.buf.as_ptr().cast::<u64>().add(self.end) })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unreadable_literal)]
#[allow(clippy::zero_prefixed_literal)]
mod tests {
    use super::*;
    use crate::utils::max_pow_u64;

    // 90630363884335538722706632492458228784305343302099024356772372330524102404852
    const N: Uint<256, 4> = Uint::from_limbs([
        0xa8ec92344438aaf4_u64,
        0x9819ebdbd1faaab1_u64,
        0x573b1a7064c19c1a_u64,
        0xc85ef7d79691fe79_u64,
    ]);

    #[test]
    fn test_to_base_le() {
        assert_eq!(
            Uint::<64, 1>::from(123456789).to_base_le(10).collect::<Vec<_>>(),
            vec![9, 8, 7, 6, 5, 4, 3, 2, 1]
        );
        assert_eq!(
            N.to_base_le(10000000000000000000_u64).collect::<Vec<_>>(),
            vec![
                2372330524102404852,
                0534330209902435677,
                7066324924582287843,
                0630363884335538722,
                9
            ]
        );
    }

    #[test]
    fn test_from_base_le() {
        assert_eq!(
            Uint::<64, 1>::from_base_le(10, [9, 8, 7, 6, 5, 4, 3, 2, 1]),
            Ok(Uint::<64, 1>::from(123456789))
        );
        assert_eq!(
            Uint::<256, 4>::from_base_le(
                10000000000000000000_u64,
                [
                    2372330524102404852,
                    0534330209902435677,
                    7066324924582287843,
                    0630363884335538722,
                    9
                ]
            )
            .unwrap(),
            N
        );
    }

    #[test]
    fn test_to_base_be() {
        assert_eq!(
            Uint::<64, 1>::from(123456789).to_base_be(10).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
        assert_eq!(
            N.to_base_be(10000000000000000000_u64).collect::<Vec<_>>(),
            vec![
                9,
                0630363884335538722,
                7066324924582287843,
                0534330209902435677,
                2372330524102404852
            ]
        );
    }

    #[test]
    fn test_to_base_be_2() {
        assert_eq!(
            Uint::<64, 1>::from(123456789).to_base_be_2(10).collect::<Vec<_>>(),
            vec![123456789]
        );
        assert_eq!(
            N.to_base_be_2(10000000000000000000_u64).collect::<Vec<_>>(),
            vec![
                9,
                0630363884335538722,
                7066324924582287843,
                0534330209902435677,
                2372330524102404852
            ]
        );
    }

    #[test]
    fn test_from_base_be() {
        assert_eq!(
            Uint::<64, 1>::from_base_be(10, [1, 2, 3, 4, 5, 6, 7, 8, 9]),
            Ok(Uint::<64, 1>::from(123456789))
        );
        assert_eq!(
            Uint::<256, 4>::from_base_be(
                10000000000000000000_u64,
                [
                    9,
                    0630363884335538722,
                    7066324924582287843,
                    0534330209902435677,
                    2372330524102404852
                ]
            )
            .unwrap(),
            N
        );
    }

    #[test]
    fn test_from_base_be_overflow() {
        assert_eq!(Uint::<0, 0>::from_base_be(10, core::iter::empty()), Ok(Uint::<0, 0>::ZERO));
        assert_eq!(Uint::<0, 0>::from_base_be(10, core::iter::once(0)), Ok(Uint::<0, 0>::ZERO));
        assert_eq!(
            Uint::<0, 0>::from_base_be(10, core::iter::once(1)),
            Err(BaseConvertError::Overflow)
        );
        assert_eq!(
            Uint::<1, 1>::from_base_be(10, [1, 0, 0].into_iter()),
            Err(BaseConvertError::Overflow)
        );
    }

    #[test]
    fn test_roundtrip() {
        fn test<const BITS: usize, const LIMBS: usize>(n: Uint<BITS, LIMBS>, base: u64) {
            assert_eq!(
                n.to_base_be(base).collect::<Vec<_>>(),
                n.to_base_le(base).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
            );

            let digits = n.to_base_le(base);
            let n2 = Uint::<BITS, LIMBS>::from_base_le(base, digits).unwrap();
            assert_eq!(n, n2);

            let digits = n.to_base_be(base);
            let n2 = Uint::<BITS, LIMBS>::from_base_be(base, digits).unwrap();
            assert_eq!(n, n2);

            let digits = n.to_base_be_2(base).collect::<Vec<_>>();
            let n2 = Uint::<BITS, LIMBS>::from_base_be(max_pow_u64(base), digits).unwrap();
            assert_eq!(n, n2);
        }

        let single = |x: u64| x..=x;
        for base in
            [2..=129, single(1 << 31), single(1 << 32), single(1 << 33)].into_iter().flatten()
        {
            test(Uint::<64, 1>::from(123456789), base);
            test(Uint::<128, 2>::from(123456789), base);
            test(N, base);
        }
    }
}
