// FEATURE: (BLOCKED) It would be nice to impl From<_> as well, but then the
// generic implementation `impl<T: Into<U>, U> TryFrom<U> for T` conflicts with
// our own implementation. This means we can only implement one.
// In principle this can be worked around by `specialization`, but that
// triggers other compiler issues at the moment.

// impl<T, const BITS: usize> From<T> for Uint<BITS>
// where
//     [(); nlimbs(BITS)]:,
//     Uint<BITS>: TryFrom<T>,
// {
//     fn from(t: T) -> Self {
//         Self::try_from(t).unwrap()
//     }
// }
// See <https://github.com/rust-lang/rust/issues/50133>

// FEATURE: (BLOCKED) It would be nice if we could make TryFrom assignment work
// for all Uints.
// impl<
//         const BITS_SRC: usize,
//         const LIMBS_SRC: usize,
//         const BITS_DST: usize,
//         const LIMBS_DST: usize,
//     > TryFrom<Uint<BITS_SRC, LIMBS_SRC>> for Uint<BITS_DST, LIMBS_DST>
// {
//     type Error = ToUintError;

//     fn try_from(value: Uint<BITS_SRC, LIMBS_SRC>) -> Result<Self,
// Self::Error> {
//     }
// }

use core::{fmt, fmt::Debug};

use crate::Uint;

/// Error for [`TryFrom<T>`][TryFrom] for [`Uint`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ToUintError<T> {
    /// Value is too large to fit the Uint.
    ///
    /// `.0` is `BITS` and `.1` is the wrapped value.
    ValueTooLarge(usize, T),

    /// Negative values can not be represented as Uint.
    ///
    /// `.0` is `BITS` and `.1` is the wrapped value.
    ValueNegative(usize, T),

    /// 'Not a number' (NaN) can not be represented as Uint
    NotANumber(usize),
}

#[cfg(feature = "std")]
impl<T: fmt::Debug> std::error::Error for ToUintError<T> {}

impl<T> fmt::Display for ToUintError<T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueTooLarge(bits, _) => write!(f, "Value is too large for Uint<{bits}>"),
            Self::ValueNegative(bits, _) => {
                write!(f, "Negative values cannot be represented as Uint<{bits}>")
            }
            Self::NotANumber(bits) => {
                write!(f, "'Not a number' (NaN) cannot be represented as Uint<{bits}>")
            }
        }
    }
}

/// Error for [`TryFrom<Uint>`][TryFrom].
#[allow(clippy::derive_partial_eq_without_eq)] // False positive
#[allow(clippy::module_name_repetitions)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FromUintError<T> {
    /// The Uint value is too large for the target type.
    ///
    /// `.0` number of `BITS` in the Uint, `.1` is the wrapped value and
    /// `.2` is the maximum representable value in the target type.
    Overflow(usize, T, T),
}

#[cfg(feature = "std")]
impl<T: fmt::Debug> std::error::Error for FromUintError<T> {}

impl<T> fmt::Display for FromUintError<T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow(bits, ..) => {
                write!(f, "Uint<{bits}> value is too large for {}", core::any::type_name::<T>())
            }
        }
    }
}

/// Error for [`TryFrom<Uint>`][TryFrom] for [`ark_ff`](https://docs.rs/ark-ff) and others.
#[allow(dead_code)] // This is used by some support features.
#[derive(Debug, Clone, Copy)]
pub enum ToFieldError {
    /// Number is equal or larger than the target field modulus.
    NotInField,
}

#[cfg(feature = "std")]
impl std::error::Error for ToFieldError {}

impl fmt::Display for ToFieldError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInField => {
                f.write_str("Number is equal or larger than the target field modulus.")
            }
        }
    }
}

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Constructs a new [`Uint`] from a u64.
    ///
    /// Saturates at the maximum value of the [`Uint`] if the value is too
    /// large.
    pub(crate) const fn const_from_u64(x: u64) -> Self {
        if BITS == 0 || (BITS < 64 && x >= 1 << BITS) {
            return Self::MAX;
        }
        let mut limbs = [0; LIMBS];
        limbs[0] = x;
        Self::from_limbs(limbs)
    }

    /// Construct a new [`Uint`] from the value.
    ///
    /// # Panics
    ///
    /// Panics if the conversion fails, for example if the value is too large
    /// for the bit-size of the [`Uint`]. The panic will be attributed to the
    /// call site.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruint::{Uint, uint, aliases::*};
    /// # uint!{
    /// assert_eq!(U8::from(142_u16), 142_U8);
    /// assert_eq!(U64::from(0x7014b4c2d1f2_U256), 0x7014b4c2d1f2_U64);
    /// assert_eq!(U64::from(3.145), 3_U64);
    /// # }
    /// ```
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn from<T>(value: T) -> Self
    where
        Self: UintTryFrom<T>,
    {
        match Self::uint_try_from(value) {
            Ok(n) => n,
            Err(e) => panic!("Uint conversion error: {e}"),
        }
    }

    /// Construct a new [`Uint`] from the value saturating the value to the
    /// minimum or maximum value of the [`Uint`].
    ///
    /// If the value is not a number (like `f64::NAN`), then the result is
    /// set zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruint::{Uint, uint, aliases::*};
    /// # uint!{
    /// assert_eq!(U8::saturating_from(300_u16), 255_U8);
    /// assert_eq!(U8::saturating_from(-10_i16), 0_U8);
    /// assert_eq!(U32::saturating_from(0x7014b4c2d1f2_U256), U32::MAX);
    /// # }
    /// ```
    #[inline]
    #[must_use]
    pub fn saturating_from<T>(value: T) -> Self
    where
        Self: UintTryFrom<T>,
    {
        match Self::uint_try_from(value) {
            Ok(n) => n,
            Err(ToUintError::ValueTooLarge(..)) => Self::MAX,
            Err(ToUintError::ValueNegative(..) | ToUintError::NotANumber(_)) => Self::ZERO,
        }
    }

    /// Construct a new [`Uint`] from the value saturating the value to the
    /// minimum or maximum value of the [`Uint`].
    ///
    /// If the value is not a number (like `f64::NAN`), then the result is
    /// set zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruint::{Uint, uint, aliases::*};
    /// # uint!{
    /// assert_eq!(U8::wrapping_from(300_u16), 44_U8);
    /// assert_eq!(U8::wrapping_from(-10_i16), 246_U8);
    /// assert_eq!(U32::wrapping_from(0x7014b4c2d1f2_U256), 0xb4c2d1f2_U32);
    /// # }
    /// ```
    #[inline]
    #[must_use]
    pub fn wrapping_from<T>(value: T) -> Self
    where
        Self: UintTryFrom<T>,
    {
        match Self::uint_try_from(value) {
            Ok(n) | Err(ToUintError::ValueTooLarge(_, n) | ToUintError::ValueNegative(_, n)) => n,
            Err(ToUintError::NotANumber(_)) => Self::ZERO,
        }
    }

    /// # Panics
    ///
    /// Panics if the conversion fails, for example if the value is too large
    /// for the bit-size of the target type.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ruint::{Uint, uint, aliases::*};
    /// # uint!{
    /// assert_eq!(300_U12.to::<i16>(), 300_i16);
    /// assert_eq!(300_U12.to::<U256>(), 300_U256);
    /// # }
    /// ```
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn to<T>(&self) -> T
    where
        Self: UintTryTo<T>,
        T: Debug,
    {
        self.uint_try_to().expect("Uint conversion error")
    }

    /// # Examples
    ///
    /// ```
    /// # use ruint::{Uint, uint, aliases::*};
    /// # uint!{
    /// assert_eq!(300_U12.wrapping_to::<i8>(), 44_i8);
    /// assert_eq!(255_U32.wrapping_to::<i8>(), -1_i8);
    /// assert_eq!(0x1337cafec0d3_U256.wrapping_to::<U32>(), 0xcafec0d3_U32);
    /// # }
    /// ```
    #[inline]
    #[must_use]
    pub fn wrapping_to<T>(&self) -> T
    where
        Self: UintTryTo<T>,
    {
        match self.uint_try_to() {
            Ok(n) | Err(FromUintError::Overflow(_, n, _)) => n,
        }
    }

    /// # Examples
    ///
    /// ```
    /// # use ruint::{Uint, uint, aliases::*};
    /// # uint!{
    /// assert_eq!(300_U12.saturating_to::<i16>(), 300_i16);
    /// assert_eq!(255_U32.saturating_to::<i8>(), 127);
    /// assert_eq!(0x1337cafec0d3_U256.saturating_to::<U32>(), U32::MAX);
    /// # }
    /// ```
    #[inline]
    #[must_use]
    pub fn saturating_to<T>(&self) -> T
    where
        Self: UintTryTo<T>,
    {
        match self.uint_try_to() {
            Ok(n) | Err(FromUintError::Overflow(_, _, n)) => n,
        }
    }

    /// Construct a new [`Uint`] from a potentially different sized [`Uint`].
    ///
    /// # Panics
    ///
    /// Panics if the value is too large for the target type.
    #[inline]
    #[doc(hidden)]
    #[must_use]
    #[track_caller]
    #[deprecated(since = "1.4.0", note = "Use `::from()` instead.")]
    pub fn from_uint<const BITS_SRC: usize, const LIMBS_SRC: usize>(
        value: Uint<BITS_SRC, LIMBS_SRC>,
    ) -> Self {
        Self::from_limbs_slice(value.as_limbs())
    }

    #[inline]
    #[doc(hidden)]
    #[must_use]
    #[deprecated(since = "1.4.0", note = "Use `::checked_from()` instead.")]
    pub fn checked_from_uint<const BITS_SRC: usize, const LIMBS_SRC: usize>(
        value: Uint<BITS_SRC, LIMBS_SRC>,
    ) -> Option<Self> {
        Self::checked_from_limbs_slice(value.as_limbs())
    }

    /// Returns `true` if `self` is larger than 64 bits.
    #[inline]
    fn gt_u64_max(&self) -> bool {
        self.limbs_gt(1)
    }

    /// Returns `true` if `self` is larger than 128 bits.
    #[inline]
    fn gt_u128_max(&self) -> bool {
        self.limbs_gt(2)
    }

    /// Returns `true` if `self` is larger than `64 * n` bits.
    #[inline]
    fn limbs_gt(&self, n: usize) -> bool {
        if LIMBS < n {
            return false;
        }

        if BITS <= 512 {
            // Use branchless `bitor` chain for smaller integers.
            self.as_limbs()[n..].iter().copied().fold(0u64, core::ops::BitOr::bitor) != 0
        } else {
            self.bit_len() > 64 * n
        }
    }
}

/// ⚠️ Workaround for [Rust issue #50133](https://github.com/rust-lang/rust/issues/50133).
/// Use [`TryFrom`] instead.
///
/// We cannot implement [`TryFrom<Uint>`] for [`Uint`] directly, but we can
/// create a new identical trait and implement it there. We can even give this
/// trait a blanket implementation inheriting all [`TryFrom<_>`]
/// implementations.
#[allow(clippy::module_name_repetitions)]
pub trait UintTryFrom<T>: Sized {
    #[doc(hidden)]
    fn uint_try_from(value: T) -> Result<Self, ToUintError<Self>>;
}

/// Blanket implementation for any type that implements [`TryFrom<Uint>`].
impl<const BITS: usize, const LIMBS: usize, T> UintTryFrom<T> for Uint<BITS, LIMBS>
where
    Self: TryFrom<T, Error = ToUintError<Self>>,
{
    #[inline]
    fn uint_try_from(value: T) -> Result<Self, ToUintError<Self>> {
        Self::try_from(value)
    }
}

impl<const BITS: usize, const LIMBS: usize, const BITS_SRC: usize, const LIMBS_SRC: usize>
    UintTryFrom<Uint<BITS_SRC, LIMBS_SRC>> for Uint<BITS, LIMBS>
{
    #[inline]
    fn uint_try_from(value: Uint<BITS_SRC, LIMBS_SRC>) -> Result<Self, ToUintError<Self>> {
        let (n, overflow) = Self::overflowing_from_limbs_slice(value.as_limbs());
        if overflow { Err(ToUintError::ValueTooLarge(BITS, n)) } else { Ok(n) }
    }
}

/// ⚠️ Workaround for [Rust issue #50133](https://github.com/rust-lang/rust/issues/50133).
/// Use [`TryFrom`] instead.
pub trait UintTryTo<T>: Sized {
    #[doc(hidden)]
    fn uint_try_to(&self) -> Result<T, FromUintError<T>>;
}

impl<const BITS: usize, const LIMBS: usize, T> UintTryTo<T> for Uint<BITS, LIMBS>
where
    T: for<'a> TryFrom<&'a Self, Error = FromUintError<T>>,
{
    #[inline]
    fn uint_try_to(&self) -> Result<T, FromUintError<T>> {
        T::try_from(self)
    }
}

impl<const BITS: usize, const LIMBS: usize, const BITS_DST: usize, const LIMBS_DST: usize>
    UintTryTo<Uint<BITS_DST, LIMBS_DST>> for Uint<BITS, LIMBS>
{
    #[inline]
    fn uint_try_to(
        &self,
    ) -> Result<Uint<BITS_DST, LIMBS_DST>, FromUintError<Uint<BITS_DST, LIMBS_DST>>> {
        let (n, overflow) = Uint::overflowing_from_limbs_slice(self.as_limbs());
        if overflow { Err(FromUintError::Overflow(BITS_DST, n, Uint::MAX)) } else { Ok(n) }
    }
}

// u64 is a single limb, so this is the base case
impl<const BITS: usize, const LIMBS: usize> TryFrom<u64> for Uint<BITS, LIMBS> {
    type Error = ToUintError<Self>;

    #[inline]
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match LIMBS {
            0 | 1 if value > Self::MASK => {
                return Err(ToUintError::ValueTooLarge(
                    BITS,
                    Self::from_limbs([value & Self::MASK; LIMBS]),
                ));
            }
            0 => return Ok(Self::ZERO),
            _ => {}
        }
        let mut limbs = [0; LIMBS];
        limbs[0] = value;
        Ok(Self::from_limbs(limbs))
    }
}

// u128 version is handled specially in because it covers two limbs.
impl<const BITS: usize, const LIMBS: usize> TryFrom<u128> for Uint<BITS, LIMBS> {
    type Error = ToUintError<Self>;

    #[inline]
    #[allow(clippy::cast_lossless)]
    #[allow(clippy::cast_possible_truncation)]
    fn try_from(value: u128) -> Result<Self, Self::Error> {
        if value <= u64::MAX as u128 {
            return Self::try_from(value as u64);
        }
        if LIMBS < 2 {
            return Self::try_from(value as u64)
                .and_then(|n| Err(ToUintError::ValueTooLarge(BITS, n)));
        }
        let mut limbs = [0; LIMBS];
        limbs[0] = value as u64;
        limbs[1] = (value >> 64) as u64;
        if LIMBS == 2 && limbs[1] > Self::MASK {
            limbs[1] &= Self::MASK;
            Err(ToUintError::ValueTooLarge(BITS, Self::from_limbs(limbs)))
        } else {
            Ok(Self::from_limbs(limbs))
        }
    }
}

// Unsigned int version upcast to u64
macro_rules! impl_from_unsigned_int {
    ($uint:ty) => {
        impl<const BITS: usize, const LIMBS: usize> TryFrom<$uint> for Uint<BITS, LIMBS> {
            type Error = ToUintError<Self>;

            #[inline]
            fn try_from(value: $uint) -> Result<Self, Self::Error> {
                Self::try_from(value as u64)
            }
        }
    };
}

impl_from_unsigned_int!(bool);
impl_from_unsigned_int!(u8);
impl_from_unsigned_int!(u16);
impl_from_unsigned_int!(u32);
impl_from_unsigned_int!(usize);

// Signed int version check for positive and delegate to the corresponding
// `uint`.
macro_rules! impl_from_signed_int {
    ($int:ty, $uint:ty) => {
        impl<const BITS: usize, const LIMBS: usize> TryFrom<$int> for Uint<BITS, LIMBS> {
            type Error = ToUintError<Self>;

            #[inline]
            fn try_from(value: $int) -> Result<Self, Self::Error> {
                if value.is_negative() {
                    Err(match Self::try_from(value as $uint) {
                        Ok(n) | Err(ToUintError::ValueTooLarge(_, n)) => {
                            ToUintError::ValueNegative(BITS, n)
                        }
                        _ => unreachable!(),
                    })
                } else {
                    Self::try_from(value as $uint)
                }
            }
        }
    };
}

impl_from_signed_int!(i8, u8);
impl_from_signed_int!(i16, u16);
impl_from_signed_int!(i32, u32);
impl_from_signed_int!(i64, u64);
impl_from_signed_int!(i128, u128);
impl_from_signed_int!(isize, usize);

impl<const BITS: usize, const LIMBS: usize> TryFrom<f64> for Uint<BITS, LIMBS> {
    type Error = ToUintError<Self>;

    #[inline]
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        // Mimics Rust's own float-to-int conversion
        // https://github.com/rust-lang/compiler-builtins/blob/f4c7940d3b13ec879c9fdc218812f71a65149123/src/float/conv.rs#L163

        let f = value;
        let fixint_bits = Self::BITS;

        let sign_bit = 0x8000_0000_0000_0000u64;
        let significand_bits = 52usize;
        let exponent_bias = 1023usize;
        const HALF_BITS: u64 = 0.5f64.to_bits();

        // Break into sign, exponent, significand
        let a_rep = f.to_bits();
        let a_abs = a_rep & !sign_bit;

        let sign = if (a_rep & sign_bit) == 0 {
            if a_rep < HALF_BITS {
                return Ok(Self::ZERO);
            }
            Sign::Positive
        } else {
            if a_abs == 0 {
                return Ok(Self::ZERO);
            }
            Sign::Negative
        };
        let mut exponent = (a_abs >> significand_bits) as usize;
        let significand = (a_abs & ((1u64 << significand_bits) - 1)) | (1u64 << significand_bits);

        let from_lossy = |x| match Self::uint_try_from(x) {
            Ok(n) => n,
            Err(ToUintError::ValueTooLarge(_, n)) => n,
            _ => unreachable!(),
        };

        // Helper: produce integer magnitude for |f| given an unbiased exponent `e`,
        // using round-to-nearest, ties-to-even, then interpreted modulo 2^BITS by Uint.
        let compute_mag = |e: usize| -> Self {
            if e < significand_bits {
                // Right shift with round-to-nearest, ties-to-even
                let shift = significand_bits - e; // shift >= 1 here
                let mut r = significand >> shift;
                let remainder = significand & ((1u64 << shift) - 1);
                let halfway = 1u64 << (shift - 1);
                if remainder > halfway || (remainder == halfway && (r & 1) == 1) {
                    r = r.wrapping_add(1);
                }
                from_lossy(r)
            } else {
                // Left shift; Uint shifts are modulo 2^BITS already.
                from_lossy(significand).wrapping_shl(e - significand_bits)
            }
        };

        // Negative values: return ValueNegative with wrapped two's-complement payload.
        // Handle |value| < 1 without going through the saturating exponent path to
        // preserve correct rounding: ties-to-even at 0.5, otherwise nearest.
        if sign == Sign::Negative {
            if exponent < exponent_bias {
                // |value| < 1
                // Rounds to 0 for −0.5 ≤ value < 0.0, and to 1 for −1.0 < value < −0.5.
                if value >= -0.5 {
                    return Err(ToUintError::ValueNegative(BITS, Self::ZERO));
                } else {
                    let wrapped = Self::ZERO.wrapping_sub(Self::ONE);
                    return Err(ToUintError::ValueNegative(BITS, wrapped));
                }
            } else {
                // |value| >= 1: compute magnitude normally with unbiased exponent.
                let e = exponent - exponent_bias;
                let mag = compute_mag(e);
                let wrapped = Self::ZERO.wrapping_sub(mag);
                return Err(ToUintError::ValueNegative(BITS, wrapped));
            }
        }

        // Positive and exponent indicates |value| < 1
        if exponent < exponent_bias {
            return if BITS == 0 {
                Err(ToUintError::ValueTooLarge(BITS, Self::ZERO))
            } else {
                // We already handled value < 0.5 above; here 0.5 <= value < 1.0 → 1.
                Ok(Self::ONE)
            };
        }
        exponent -= exponent_bias;

        // If the value is infinity, saturate.
        // If the value is too large for the integer type, wrap (drop high bits) and
        // return it in the error.
        if exponent >= fixint_bits {
            if value.is_nan() {
                return Err(ToUintError::NotANumber(BITS));
            }

            let mag = compute_mag(exponent);
            let wrapped = match sign {
                Sign::Positive => mag,
                Sign::Negative => Self::ZERO.wrapping_sub(mag),
            };
            return match sign {
                Sign::Positive => Err(ToUintError::ValueTooLarge(BITS, wrapped)),
                Sign::Negative => Err(ToUintError::ValueNegative(BITS, wrapped)),
            };
        }

        // In-range: produce the integer normally.
        let r = compute_mag(exponent);

        // Match old impl: if rounding bumps us across 2^BITS (only possible when
        // exponent == BITS - 1), report ValueTooLarge with the wrapped payload.
        if sign == Sign::Positive
            && fixint_bits > 0
            && exponent == fixint_bits - 1
            && r == Self::ZERO
        {
            return Err(ToUintError::ValueTooLarge(BITS, r));
        }

        Ok(r)
    }
}

#[derive(PartialEq)]
enum Sign {
    Positive,
    Negative,
}

impl<const BITS: usize, const LIMBS: usize> TryFrom<f32> for Uint<BITS, LIMBS> {
    type Error = ToUintError<Self>;

    #[inline]
    fn try_from(value: f32) -> Result<Self, Self::Error> {
        #[allow(clippy::cast_lossless)]
        Self::try_from(value as f64)
    }
}

// Convert Uint to integer types

// Required because a generic rule violates the orphan rule
macro_rules! to_value_to_ref {
    ($t:ty) => {
        impl<const BITS: usize, const LIMBS: usize> TryFrom<Uint<BITS, LIMBS>> for $t {
            type Error = FromUintError<Self>;

            #[inline]
            fn try_from(value: Uint<BITS, LIMBS>) -> Result<Self, Self::Error> {
                Self::try_from(&value)
            }
        }
    };
}

to_value_to_ref!(bool);

impl<const BITS: usize, const LIMBS: usize> TryFrom<&Uint<BITS, LIMBS>> for bool {
    type Error = FromUintError<Self>;

    #[inline]
    fn try_from(value: &Uint<BITS, LIMBS>) -> Result<Self, Self::Error> {
        if BITS == 0 {
            return Ok(false);
        }
        if value.gt_u64_max() || value.limbs[0] > 1 {
            return Err(Self::Error::Overflow(BITS, value.bit(0), true));
        }
        Ok(value.limbs[0] != 0)
    }
}

macro_rules! to_int {
    ($($int:ty)*) => {$(
        to_value_to_ref!($int);

        impl<const BITS: usize, const LIMBS: usize> TryFrom<&Uint<BITS, LIMBS>> for $int {
            type Error = FromUintError<Self>;

            #[inline]
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            fn try_from(value: &Uint<BITS, LIMBS>) -> Result<Self, Self::Error> {
                if BITS == 0 {
                    return Ok(0);
                }
                if value.gt_u64_max() || value.limbs[0] > (Self::MAX as u64) {
                    return Err(Self::Error::Overflow(
                        BITS,
                        value.limbs[0] as Self,
                        Self::MAX,
                    ));
                }
                Ok(value.limbs[0] as Self)
            }
        }
    )*};
}

to_int!(i8 u8 i16 u16 i32 u32 i64 u64 isize usize);

to_value_to_ref!(i128);

impl<const BITS: usize, const LIMBS: usize> TryFrom<&Uint<BITS, LIMBS>> for i128 {
    type Error = FromUintError<Self>;

    #[inline]
    #[allow(clippy::cast_possible_wrap)] // Intentional.
    #[allow(clippy::cast_lossless)] // Safe casts
    #[allow(clippy::use_self)] // More readable
    fn try_from(value: &Uint<BITS, LIMBS>) -> Result<Self, Self::Error> {
        if BITS <= 64 {
            return Ok(u64::try_from(value).unwrap().into());
        }
        let result = value.as_double_words()[0].get();
        if value.gt_u128_max() || result > i128::MAX as u128 {
            return Err(Self::Error::Overflow(BITS, result as i128, i128::MAX));
        }
        Ok(result as i128)
    }
}

to_value_to_ref!(u128);

impl<const BITS: usize, const LIMBS: usize> TryFrom<&Uint<BITS, LIMBS>> for u128 {
    type Error = FromUintError<Self>;

    #[inline]
    #[allow(clippy::cast_lossless)] // Safe casts
    #[allow(clippy::use_self)] // More readable
    fn try_from(value: &Uint<BITS, LIMBS>) -> Result<Self, Self::Error> {
        if BITS <= 64 {
            return Ok(u64::try_from(value).unwrap().into());
        }
        let result = value.as_double_words()[0].get();
        if value.gt_u128_max() {
            return Err(Self::Error::Overflow(BITS, result, u128::MAX));
        }
        Ok(result)
    }
}

// Convert Uint to floating point

impl<const BITS: usize, const LIMBS: usize> From<Uint<BITS, LIMBS>> for f32 {
    #[inline]
    fn from(value: Uint<BITS, LIMBS>) -> Self {
        Self::from(&value)
    }
}

impl<const BITS: usize, const LIMBS: usize> From<&Uint<BITS, LIMBS>> for f32 {
    /// Approximate single precision float.
    ///
    /// Returns `f32::INFINITY` if the value is too large to represent.
    #[inline]
    #[allow(clippy::cast_precision_loss)] // Documented
    fn from(value: &Uint<BITS, LIMBS>) -> Self {
        f64::from(value) as f32
    }
}

impl<const BITS: usize, const LIMBS: usize> From<Uint<BITS, LIMBS>> for f64 {
    #[inline]
    fn from(value: Uint<BITS, LIMBS>) -> Self {
        Self::from(&value)
    }
}

impl<const BITS: usize, const LIMBS: usize> From<&Uint<BITS, LIMBS>> for f64 {
    /// Approximate double precision float.
    ///
    /// Returns `f64::INFINITY` if the value is too large to represent.
    #[inline]
    fn from(value: &Uint<BITS, LIMBS>) -> Self {
        Self::from_bits(value.as_f64_bits())
    }
}

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Convert to IEEE 754 double precision float bit representation.
    #[inline]
    fn as_f64_bits(&self) -> u64 {
        as_primitives!(self; {
            u64(x) => return f64::to_bits(x as f64),
        });

        const SIG: usize = f64::MANTISSA_DIGITS as usize; // includes the hidden bit

        let sd = self.bit_len(); // 0 for zero
        if sd == 0 {
            return 0;
        }

        // Early +∞ if exponent field is already saturated before rounding.
        // e_pre = 1021 + sd; saturation when e_pre >= 0x7ff <=> sd >= 1026
        if sd >= 1026 {
            return 0x7ff0_0000_0000_0000;
        }

        let e_pre = 1021u64 + sd as u64;

        // Fits entirely in the 53-bit significand: normalize, no rounding.
        if sd <= SIG {
            // value < 2^53, so it lives in limb 0
            let a = self.as_limbs().first().copied().unwrap_or(0) << (SIG - sd);
            return (e_pre << 52) + a;
        }

        // sd > SIG: need guard/sticky. Extract a 54-bit window [MSB .. MSB-53].
        let msb = sd - 1;
        let li = msb >> 6;
        let off = (msb & 63) as u32;

        let limbs = self.as_limbs();
        let hi = limbs[li];
        let lo = if li > 0 { limbs[li - 1] } else { 0 };

        // Concatenate [hi:64][lo:64] and drop the low bits so the 54-bit window is at
        // LSB. Correct alignment: shift = 64 + off - SIG  (not SIG+1!)
        let shift = 64 + off as usize - SIG; // range 11..=74
        debug_assert!((11..=74).contains(&shift));

        let w = ((hi as u128) << 64) | (lo as u128);
        let win54 = (w >> shift) as u64; // low 54 bits are [MSB .. MSB-53]

        let a = win54 >> 1; // 53-bit mantissa incl. hidden bit
        let guard = (win54 & 1) != 0;

        // Sticky = any bit strictly below guard.
        // That’s equivalent to: trailing_zeros(value) < guard_pos
        // where guard_pos = sd - SIG - 1 (# bits below guard).
        let guard_pos = sd - SIG - 1;
        let sticky = guard_pos != 0 && self.trailing_zeros() < guard_pos;

        // Round to nearest, ties-to-even.
        let round_up = guard && (sticky || ((a & 1) != 0));
        let m = a + (round_up as u64);

        // Combine with '+' so a carry out of m bumps the exponent.
        (e_pre << 52) + m
    }
}

#[cfg(test)]
mod test {
    use proptest::proptest;

    use super::*;
    use crate::{const_for, nlimbs};

    #[test]
    fn test_u64() {
        assert_eq!(Uint::<0, 0>::try_from(0_u64), Ok(Uint::ZERO));
        assert_eq!(Uint::<0, 0>::try_from(1_u64), Err(ToUintError::ValueTooLarge(0, Uint::ZERO)));
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            assert_eq!(Uint::<BITS, LIMBS>::try_from(0_u64), Ok(Uint::ZERO));
            assert_eq!(Uint::<BITS, LIMBS>::try_from(1_u64).unwrap().as_limbs()[0], 1);
        });
    }

    #[test]
    fn test_u64_max() {
        assert_eq!(Uint::<64, 1>::try_from(u64::MAX), Ok(Uint::from_limbs([u64::MAX])));
        assert_eq!(Uint::<64, 1>::try_from(u64::MAX as u128), Ok(Uint::from_limbs([u64::MAX])));
        assert_eq!(
            Uint::<64, 1>::try_from(u64::MAX as u128 + 1),
            Err(ToUintError::ValueTooLarge(64, Uint::ZERO))
        );

        assert_eq!(Uint::<128, 2>::try_from(u64::MAX), Ok(Uint::from_limbs([u64::MAX, 0])));
        assert_eq!(Uint::<128, 2>::try_from(u64::MAX as u128), Ok(Uint::from_limbs([u64::MAX, 0])));
        assert_eq!(Uint::<128, 2>::try_from(u64::MAX as u128 + 1), Ok(Uint::from_limbs([0, 1])));
    }

    #[test]
    fn test_u65() {
        let x = uint!(18446744073711518810_U65);
        assert_eq!(x.bit_len(), 65);
        assert_eq!(u64::try_from(x), Err(FromUintError::Overflow(65, 1967194, u64::MAX)));
    }

    #[test]
    fn test_f64() {
        assert_eq!(Uint::<0, 0>::try_from(0.0_f64), Ok(Uint::ZERO));
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            assert_eq!(Uint::<BITS, LIMBS>::try_from(0.0_f64), Ok(Uint::ZERO));
            assert_eq!(Uint::<BITS, LIMBS>::try_from(1.0_f64).unwrap().as_limbs()[0], 1);
            assert_eq!(Uint::<BITS, LIMBS>::try_from(-1.0_f64), old_uint_try_from::<BITS, LIMBS>(-1.0_f64));
        });
        assert_eq!(Uint::<7, 1>::try_from(123.499_f64), Ok(Uint::from_limbs([123])));
        assert_eq!(Uint::<7, 1>::try_from(123.500_f64), Ok(Uint::from_limbs([124])));
    }

    #[test]
    fn all_integers_are_representable() {
        const MAX_SAFE_INTEGER: u64 = (1 << f64::MANTISSA_DIGITS) - 1;
        proptest!(|(value in 0..=MAX_SAFE_INTEGER)| {
            let from_float = Uint::<64, 1>::try_from(value as f64).unwrap();
            let from_int = Uint::<64, 1>::try_from(value).unwrap();
            assert_eq!(from_float, from_int);
        });
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_old_new_impl_from_f64_equivalent() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(value: f64)| {
                let old = old_uint_try_from::<BITS, LIMBS>(value);
                let new = Uint::<BITS, LIMBS>::try_from(value);
                match (old, new) {
                    (Ok(expected), Ok(actual)) => {
                        assert!(
                            expected == actual || (expected == actual + Uint::ONE),
                            "assertion failed: `(expected == actual)`\nexpected: {:?}\n  actual: {:?}\n{}",
                            expected,
                            actual,
                            format_args!("BITS = {BITS}, value = {value}")
                        )
                    }
                    (Err(ToUintError::ValueTooLarge(_, expected)), Err(ToUintError::ValueTooLarge(_, actual))) => {
                        assert!(
                            expected == actual || (expected == actual + Uint::ONE),
                            "assertion failed: `(expected == actual)`\nexpected: {:?}\n  actual: {:?}\n{}",
                            expected,
                            actual,
                            format_args!("BITS = {BITS}, value = {value}")
                        )
                    }
                    (Err(ToUintError::ValueNegative(_, expected)), Err(ToUintError::ValueNegative(_, actual))) => {
                        assert!(
                            expected == actual || (expected == actual + Uint::ONE),
                            "assertion failed: `(expected == actual)`\nexpected: {:?}\n  actual: {:?}\n{}",
                            expected,
                            actual,
                            format_args!("BITS = {BITS}, value = {value}")
                        )
                    }
                    (old, new) => assert_eq!(old, new, "BITS = {BITS}, value = {value}")
                };
            });
        });
    }

    #[test]
    fn all_floats_are_representable() {
        const MAX_SAFE_INTEGER: u64 = (1 << f64::MANTISSA_DIGITS) - 1;
        proptest!(|(value in 0..=MAX_SAFE_INTEGER)| {
            let uint = Uint::<64, 1>::try_from(value).unwrap();

            let old = value as f64;
            let new = f64::from(&uint);
            assert_eq!(old, new);
        });
    }

    #[test]
    fn small_uints_work_correctly() {
        const_for!(BITS in [1, 2, 3, 4, 5, 6, 7, 8, 16, 24, 32, 48, 64] {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(value in 0..(1u128 << BITS))| {
                let uint = Uint::<BITS, LIMBS>::try_from(value).unwrap();

                let old = value as f64;
                let new = f64::from(&uint);
                assert_eq!(old, new);

                let old = value as f32;
                let new = f32::from(&uint);
                assert_eq!(old, new);
            });
        });
    }

    #[test]
    fn number_too_large_is_infinity() {
        const F64_BITS: usize = 1100;
        const F_64LIMBS: usize = nlimbs(F64_BITS);

        const F32_BITS: usize = F64_BITS / 2;
        const F_32LIMBS: usize = nlimbs(F32_BITS);

        assert!(f64::from(Uint::<F64_BITS, F_64LIMBS>::MAX).is_infinite());
        assert!(f32::from(Uint::<F32_BITS, F_32LIMBS>::MAX).is_infinite());
    }

    #[cfg(feature = "std")]
    fn old_uint_to_f64<const BITS: usize, const LIMBS: usize>(value: &Uint<BITS, LIMBS>) -> f64 {
        let (bits, exponent) = value.most_significant_bits();
        (bits as f64) * (exponent as f64).exp2()
    }

    #[cfg(feature = "std")]
    fn old_uint_to_f32<const BITS: usize, const LIMBS: usize>(value: &Uint<BITS, LIMBS>) -> f32 {
        let (bits, exponent) = value.most_significant_bits();
        (bits as f32) * (exponent as f32).exp2()
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_old_new_impl_to_f64_equivalent() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint::<BITS, LIMBS>;
            proptest!(|(value: U)| {
                let expected = old_uint_to_f64::<BITS, LIMBS>(&value);
                let actual = f64::from(&value);

                let expected_bits = expected.to_bits();
                let actual_bits = actual.to_bits();
                assert!(
                    expected_bits == actual_bits || (expected_bits == actual_bits + 1) || (expected_bits + 1 == actual_bits),
                    "assertion failed: `(expected == actual)`\nexpected: {:?}\n  actual: {:?}\n{}",
                    expected,
                    actual,
                    format_args!("BITS = {BITS}, value = {value}")
                )
            });
        });
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_old_new_impl_to_f32_equivalent() {
        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);
            type U = Uint::<BITS, LIMBS>;
            proptest!(|(value: U)| {
                let expected = old_uint_to_f32::<BITS, LIMBS>(&value);
                let actual = f32::from(&value);

                let expected_bits = expected.to_bits();
                let actual_bits = actual.to_bits();
                assert!(
                    expected_bits == actual_bits || (expected_bits == actual_bits + 1) || (expected_bits + 1 == actual_bits),
                    "assertion failed: `(expected == actual)`\nexpected: {:?}\n  actual: {:?}\n{}",
                    expected,
                    actual,
                    format_args!("BITS = {BITS}, value = {value}")
                )
            });
        });
    }

    #[cfg(feature = "std")]
    fn old_uint_try_from<const BITS: usize, const LIMBS: usize>(
        value: f64,
    ) -> Result<Uint<BITS, LIMBS>, ToUintError<Uint<BITS, LIMBS>>> {
        if value.is_nan() {
            return Err(ToUintError::NotANumber(BITS));
        }
        if value < 0.0 {
            let wrapped = match Uint::try_from(value.abs()) {
                Ok(n) | Err(ToUintError::ValueTooLarge(_, n)) => n,
                _ => Uint::ZERO,
            }
            .wrapping_neg();
            return Err(ToUintError::ValueNegative(BITS, wrapped));
        }
        #[allow(clippy::cast_precision_loss)] // BITS is small-ish
        let modulus = (BITS as f64).exp2();
        if value >= modulus {
            let wrapped = match Uint::try_from(value % modulus) {
                Ok(n) | Err(ToUintError::ValueTooLarge(_, n)) => n,
                _ => Uint::ZERO,
            };
            return Err(ToUintError::ValueTooLarge(BITS, wrapped)); // Wrapping
        }
        if value < 0.5 {
            return Ok(Uint::ZERO);
        }
        // All non-normal cases should have been handled above
        assert!(value.is_normal());

        // Add offset to round to nearest integer.
        let value = value + 0.5;

        // Parse IEEE-754 double
        // Sign should be zero, exponent should be >= 0.
        let bits = value.to_bits();
        let sign = bits >> 63;
        assert!(sign == 0);
        let biased_exponent = (bits >> 52) & 0x7ff;
        assert!(biased_exponent >= 1023);
        let exponent = biased_exponent - 1023;
        let fraction = bits & 0x000f_ffff_ffff_ffff;
        let mantissa = 0x0010_0000_0000_0000 | fraction;

        // Convert mantissa * 2^(exponent - 52) to Uint
        #[allow(clippy::cast_possible_truncation)] // exponent is small-ish
        if exponent as usize > BITS + 52 {
            // Wrapped value is zero because the value is extended with zero bits.
            return Err(ToUintError::ValueTooLarge(BITS, Uint::ZERO));
        }
        if exponent <= 52 {
            // Truncate mantissa
            Uint::try_from(mantissa >> (52 - exponent))
        } else {
            #[allow(clippy::cast_possible_truncation)] // exponent is small-ish
            let exponent = exponent as usize - 52;
            let n = Uint::try_from(mantissa)?;
            let (n, overflow) = n.overflowing_shl(exponent);
            if overflow { Err(ToUintError::ValueTooLarge(BITS, n)) } else { Ok(n) }
        }
    }
}
