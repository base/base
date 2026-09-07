#![allow(clippy::missing_inline_in_public_items)] // allow format functions

use core::{fmt, str::FromStr};

use crate::{Uint, algorithms::DW, base_convert::BaseConvertError};

/// Error for [`from_str_radix`](Uint::from_str_radix).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Invalid digit in string.
    InvalidDigit(char),

    /// Invalid radix, up to base 64 is supported.
    InvalidRadix(u64),

    /// Error from [`Uint::from_base_be`].
    BaseConvertError(BaseConvertError),
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {
    #[inline]
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BaseConvertError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<BaseConvertError> for ParseError {
    #[inline]
    fn from(value: BaseConvertError) -> Self {
        Self::BaseConvertError(value)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BaseConvertError(e) => e.fmt(f),
            Self::InvalidDigit(c) => write!(f, "invalid digit: {c}"),
            Self::InvalidRadix(r) => write!(f, "invalid radix {r}, up to 64 is supported"),
        }
    }
}

/// Returns `(base, power)` where `base = radix^power` is the largest power of
/// `radix` that fits in a `u64`.
const fn radix_base(radix: u64) -> (u64, usize) {
    debug_assert!(radix >= 2);
    let mut power: usize = 1;
    let mut base = radix;
    loop {
        match base.checked_mul(radix) {
            Some(n) => {
                base = n;
                power += 1;
            }
            None => return (base, power),
        }
    }
}

/// Decode an ASCII byte as a digit for radix <= 36.
/// Case-insensitive 0-9, a-z. Underscores are skipped.
#[inline(always)]
const fn decode_digit(b: u8, radix: u64) -> Result<Option<u64>, ParseError> {
    let digit = match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'z' => b - b'a' + 10,
        b'A'..=b'Z' => b - b'A' + 10,
        b'_' => return Ok(None),
        _ => return Err(ParseError::InvalidDigit(b as char)),
    };
    let digit = digit as u64;
    if digit < radix { Ok(Some(digit)) } else { Err(ParseError::InvalidDigit(b as char)) }
}

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    /// Parse a string into a [`Uint`].
    ///
    /// For bases 2 to 36, the case-agnostic alphabet 0—1, a—b is used and `_`
    /// are ignored. For bases 37 to 64, the case-sensitive alphabet a—z, A—Z,
    /// 0—9, {+-}, {/,_} is used. That is, for base 64 it is compatible with
    /// all the common base64 variants.
    ///
    /// # Errors
    ///
    /// * [`ParseError::InvalidDigit`] if the string contains a non-digit.
    /// * [`ParseError::InvalidRadix`] if the radix is larger than 64.
    /// * [`ParseError::BaseConvertError`] if [`Uint::from_base_be`] fails.
    // FEATURE: Support proper unicode. Ignore zero-width spaces, joiners, etc.
    // Recognize digits from other alphabets.
    #[inline]
    pub const fn from_str_radix(src: &str, radix: u64) -> Result<Self, ParseError> {
        match radix {
            // Specialize for the common cases.
            2 => Self::from_str_radix_pow2(src, 2),
            8 => Self::from_str_radix_pow2(src, 8),
            10 => Self::from_str_radix_chunked(src, 10),
            16 => Self::from_str_radix_pow2(src, 16),

            65.. => Err(ParseError::InvalidRadix(radix)),
            37.. => Self::from_str_radix_slow(src, radix),
            r if r.is_power_of_two() => Self::from_str_radix_pow2(src, radix),
            _ => Self::from_str_radix_chunked(src, radix),
        }
    }

    /// Fallback for radix > 36 (base-64 alphabet). Not perf-critical.
    #[cold]
    const fn from_str_radix_slow(src: &str, radix: u64) -> Result<Self, ParseError> {
        let mut result = Self::ZERO;
        let bytes = src.as_bytes();
        const_range_for!(&b in ref bytes => {
            let digit = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'f' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' | b'-' => 62,
                b'/' | b',' | b'_' => 63,
                b'=' | b'\r' | b'\n' => continue,
                _ => return Err(ParseError::InvalidDigit(b as char)),
            };
            let digit = digit as u64;
            if digit >= radix {
                return Err(ParseError::BaseConvertError(
                    BaseConvertError::InvalidDigit(digit, radix),
                ));
            }
            match Self::muladd_limbs(&mut result.limbs, radix, digit) {
                Ok(()) => {}
                Err(e) => return Err(e),
            }
        });
        Ok(result)
    }

    /// Power-of-2 radix: shift digits directly into limbs, no multiplication.
    #[inline]
    const fn from_str_radix_pow2(src: &str, radix: u64) -> Result<Self, ParseError> {
        debug_assert!(radix.is_power_of_two());
        let bits_per_digit = radix.trailing_zeros() as usize;
        let mut result = Self::ZERO;
        let mut total_bits = 0usize;
        let bytes = src.as_bytes();
        const_range_for!(&b in rev ref bytes => {
            let digit = match decode_digit(b, radix) {
                Ok(None) => continue,
                Ok(Some(d)) => d,
                Err(e) => return Err(e),
            };
            if total_bits >= BITS {
                if digit != 0 {
                    return Err(ParseError::BaseConvertError(BaseConvertError::Overflow));
                }
                continue;
            }
            let limb_idx = total_bits / 64;
            let bit_idx = total_bits % 64;
            result.limbs[limb_idx] |= digit << bit_idx;
            if bit_idx + bits_per_digit > 64 {
                let hi = digit >> (64 - bit_idx);
                if limb_idx + 1 < LIMBS {
                    result.limbs[limb_idx + 1] |= hi;
                } else if hi != 0 {
                    return Err(ParseError::BaseConvertError(BaseConvertError::Overflow));
                }
            }
            total_bits += bits_per_digit;
        });
        if LIMBS > 0 && result.limbs[LIMBS - 1] > Self::MASK {
            return Err(ParseError::BaseConvertError(BaseConvertError::Overflow));
        }
        Ok(result)
    }

    /// Non-power-of-2 radix: accumulate chunks of digits into a u64, then do
    /// one widening multiply per chunk instead of per digit.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    const fn from_str_radix_chunked(src: &str, radix: u64) -> Result<Self, ParseError> {
        let (base, power) = radix_base(radix);
        let mut result = Self::ZERO;
        let mut chunk_val: u64 = 0;
        let mut chunk_digits: usize = 0;
        let bytes = src.as_bytes();
        const_range_for!(&b in ref bytes => {
            let digit = match decode_digit(b, radix) {
                Ok(None) => continue,
                Ok(Some(d)) => d,
                Err(e) => return Err(e),
            };
            chunk_val = chunk_val * radix + digit;
            chunk_digits += 1;
            if chunk_digits == power {
                match Self::muladd_limbs(&mut result.limbs, base, chunk_val) {
                    Ok(()) => {}
                    Err(e) => return Err(e),
                }
                chunk_val = 0;
                chunk_digits = 0;
            }
        });
        if chunk_digits > 0 {
            let mut tail_base = radix;
            const_range_for!(_i in 1..chunk_digits => {
                tail_base *= radix;
            });
            match Self::muladd_limbs(&mut result.limbs, tail_base, chunk_val) {
                Ok(()) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(result)
    }

    /// `limbs = limbs * factor + addend`, returning overflow error.
    #[inline(always)]
    const fn muladd_limbs(
        limbs: &mut [u64; LIMBS],
        factor: u64,
        addend: u64,
    ) -> Result<(), ParseError> {
        let mut carry = addend;
        const_range_for!(limbs in mut *limbs => {
            (*limbs, carry) = DW::split(DW::muladd(*limbs, factor, carry));
        });
        if carry > 0 || (LIMBS != 0 && limbs[LIMBS - 1] > Self::MASK) {
            return Err(ParseError::BaseConvertError(BaseConvertError::Overflow));
        }
        Ok(())
    }
}

impl<const BITS: usize, const LIMBS: usize> FromStr for Uint<BITS, LIMBS> {
    type Err = ParseError;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let (src, radix) = if let Some((prefix, rest)) = src.split_at_checked(2) {
            match prefix {
                "0x" | "0X" => (rest, 16),
                "0o" | "0O" => (rest, 8),
                "0b" | "0B" => (rest, 2),
                _ => (src, 10),
            }
        } else {
            (src, 10)
        };
        Self::from_str_radix(src, radix)
    }
}

#[cfg(test)]
mod tests {
    use proptest::{prop_assert_eq, proptest};

    use super::*;

    #[test]
    fn test_const_from_str_radix() {
        type U8 = Uint<8, 1>;
        type U16 = Uint<16, 1>;

        const {
            match U8::from_str_radix("1111_1111", 2) {
                Ok(value) => assert!(value.as_limbs()[0] == 255),
                Err(_) => panic!(),
            }
            match U16::from_str_radix("65_535", 10) {
                Ok(value) => assert!(value.as_limbs()[0] == 65_535),
                Err(_) => panic!(),
            }
            match U16::from_str_radix("ffff", 16) {
                Ok(value) => assert!(value.as_limbs()[0] == 65_535),
                Err(_) => panic!(),
            }
            match U8::from_str_radix("_", 64) {
                Ok(value) => assert!(value.as_limbs()[0] == 63),
                Err(_) => panic!(),
            }
            match U8::from_str_radix("256", 10) {
                Err(ParseError::BaseConvertError(BaseConvertError::Overflow)) => {}
                _ => panic!(),
            }
        }
    }

    #[test]
    fn test_from_str_radix_slow() {
        type U8 = Uint<8, 1>;

        assert_eq!(U8::from_str_radix("Z", 64), Ok(U8::from(25)));
        assert_eq!(U8::from_str_radix("a", 64), Ok(U8::from(26)));
        assert_eq!(U8::from_str_radix("f", 64), Ok(U8::from(31)));
        assert_eq!(U8::from_str_radix("0", 64), Ok(U8::from(52)));
        assert_eq!(U8::from_str_radix("9", 64), Ok(U8::from(61)));
        assert_eq!(U8::from_str_radix("+", 64), Ok(U8::from(62)));
        assert_eq!(U8::from_str_radix("-", 64), Ok(U8::from(62)));
        assert_eq!(U8::from_str_radix("/", 64), Ok(U8::from(63)));
        assert_eq!(U8::from_str_radix(",", 64), Ok(U8::from(63)));
        assert_eq!(U8::from_str_radix("_", 64), Ok(U8::from(63)));
        assert_eq!(U8::from_str_radix("=\r\n_", 64), Ok(U8::from(63)));
        assert_eq!(U8::from_str_radix("?", 64), Err(ParseError::InvalidDigit('?')));
        assert_eq!(
            U8::from_str_radix("_", 37),
            Err(ParseError::BaseConvertError(BaseConvertError::InvalidDigit(63, 37)))
        );
        assert_eq!(
            U8::from_str_radix("__", 64),
            Err(ParseError::BaseConvertError(BaseConvertError::Overflow))
        );
    }

    #[test]
    fn test_pow2_overflow() {
        type U8 = Uint<8, 1>;
        assert_eq!(U8::from_str("0xff"), Ok(U8::from(255)));
        assert_eq!(
            U8::from_str("0x1ff"),
            Err(ParseError::BaseConvertError(BaseConvertError::Overflow))
        );
        assert_eq!(
            U8::from_str("0x100"),
            Err(ParseError::BaseConvertError(BaseConvertError::Overflow))
        );

        type U7 = Uint<7, 1>;
        assert_eq!(U7::from_str("0x7f"), Ok(U7::from(127)));
        assert_eq!(
            U7::from_str("0xff"),
            Err(ParseError::BaseConvertError(BaseConvertError::Overflow))
        );

        // Octal: 0o777 = 511, which overflows U8 (max 255).
        assert_eq!(
            U8::from_str("0o777"),
            Err(ParseError::BaseConvertError(BaseConvertError::Overflow))
        );
        // Octal: 0o377 = 255, fits U8.
        assert_eq!(U8::from_str("0o377"), Ok(U8::from(255)));
    }

    #[test]
    fn test_parse() {
        proptest!(|(value: u128)| {
            type U = Uint<128, 2>;
            prop_assert_eq!(U::from_str(&format!("{value:#b}")), Ok(U::from(value)));
            prop_assert_eq!(U::from_str(&format!("{value:#o}")), Ok(U::from(value)));
            prop_assert_eq!(U::from_str(&format!("{value:}")), Ok(U::from(value)));
            prop_assert_eq!(U::from_str(&format!("{value:#x}")), Ok(U::from(value)));
            prop_assert_eq!(U::from_str(&format!("{value:#X}")), Ok(U::from(value)));
        });
    }
}
