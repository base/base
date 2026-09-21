//! Valid B-20 balance-credit recipients.

use alloy_primitives::Address;

use crate::NonZeroAddress;

/// Error returned when an address cannot receive a B-20 balance credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B20CreditRecipientError;

/// An address that can receive a B-20 balance credit.
///
/// The address is neither zero nor the token contract being credited. Policy authorization is
/// deliberately outside this type's invariant because it is operation-specific and can still
/// reject an otherwise valid credit recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct B20CreditRecipient(NonZeroAddress);

impl B20CreditRecipient {
    /// Returns a valid B-20 credit recipient, or [`B20CreditRecipientError`] when `address` is zero
    /// or is the token contract being credited.
    pub fn new(address: Address, token_address: Address) -> Result<Self, B20CreditRecipientError> {
        if address == Address::ZERO || address == token_address {
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
        assert_eq!(
            B20CreditRecipient::new(address, Address::with_last_byte(2)).unwrap().get(),
            address
        );
    }

    #[test]
    fn new_rejects_the_zero_address() {
        assert_eq!(
            B20CreditRecipient::new(Address::ZERO, Address::with_last_byte(1)),
            Err(B20CreditRecipientError)
        );
    }

    #[test]
    fn new_rejects_the_token_address() {
        let token_address = Address::with_last_byte(1);
        assert_eq!(
            B20CreditRecipient::new(token_address, token_address),
            Err(B20CreditRecipientError)
        );
    }

    #[test]
    fn new_accepts_a_different_b20_address() {
        let address = B20Variant::compute_address_for_discriminant(
            Address::repeat_byte(0x11),
            u8::MAX,
            B256::ZERO,
        )
        .0;
        assert_eq!(
            B20CreditRecipient::new(address, Address::repeat_byte(0x22)).unwrap().get(),
            address
        );
    }
}
