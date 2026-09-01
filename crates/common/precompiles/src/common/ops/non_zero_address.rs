//! Non-zero address wrapper for B-20 transfer parties.

use alloy_primitives::Address;

/// Error returned when constructing a [`NonZeroAddress`] from [`Address::ZERO`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroAddressError;

/// An [`Address`] proven not to be the zero address.
///
/// Construction fails with [`ZeroAddressError`] for the zero address. Callers map that into the
/// appropriate typed revert (`InvalidReceiver`, `InvalidSender`, …). The private field preserves
/// the invariant: there is no public way to wrap [`Address::ZERO`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonZeroAddress(Address);

impl NonZeroAddress {
    /// Returns a non-zero address, or [`ZeroAddressError`] if `address` is zero.
    pub fn new(address: Address) -> Result<Self, ZeroAddressError> {
        if address == Address::ZERO {
            Err(ZeroAddressError)
        } else {
            Ok(Self(address))
        }
    }

    /// Returns the wrapped address.
    pub const fn get(self) -> Address {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_non_zero() {
        let addr = Address::with_last_byte(1);
        assert_eq!(NonZeroAddress::new(addr).unwrap().get(), addr);
    }

    #[test]
    fn new_rejects_zero() {
        assert_eq!(NonZeroAddress::new(Address::ZERO), Err(ZeroAddressError));
    }
}
