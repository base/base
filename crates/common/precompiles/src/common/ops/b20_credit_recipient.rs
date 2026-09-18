//! Structurally valid B-20 balance-credit recipients.

use alloy_primitives::Address;

use crate::{B20Variant, NonZeroAddress};

/// Error returned when an address cannot receive a B-20 balance credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B20CreditRecipientError;

/// An address that can receive a B-20 balance credit.
///
/// The address is neither zero nor in the structural B-20 address range. Policy authorization is
/// deliberately outside this type's invariant, because it is operation-specific and can still
/// reject an otherwise valid credit recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct B20CreditRecipient(NonZeroAddress);

impl B20CreditRecipient {
    /// Returns a structurally valid B-20 credit recipient, or [`B20CreditRecipientError`] when
    /// `address` is zero or has the reserved B-20 prefix.
    pub fn new(address: Address) -> Result<Self, B20CreditRecipientError> {
        if address == Address::ZERO || B20Variant::has_b20_prefix(address) {
            return Err(B20CreditRecipientError);
        }
        NonZeroAddress::new(address).map(Self).map_err(|_| B20CreditRecipientError)
    }

    /// Returns the wrapped address.
    pub const fn get(self) -> Address {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};

    use crate::{B20CreditRecipient, B20CreditRecipientError, B20Variant};

    #[test]
    fn new_accepts_an_ordinary_address() {
        let address = Address::with_last_byte(1);
        assert_eq!(B20CreditRecipient::new(address).unwrap().get(), address);
    }

    #[test]
    fn new_rejects_the_zero_address() {
        assert_eq!(B20CreditRecipient::new(Address::ZERO), Err(B20CreditRecipientError));
    }

    #[test]
    fn new_rejects_an_uninitialized_b20_prefix_address() {
        let address =
            B20Variant::compute_address_for_discriminant(Address::repeat_byte(0x11), u8::MAX, B256::ZERO).0;
        assert_eq!(B20CreditRecipient::new(address), Err(B20CreditRecipientError));
    }
}
