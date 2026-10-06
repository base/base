//! Valid B-20 balance-credit recipients.
//!
//! [`B20CreditRecipientStrategy`] selects the rule. A changed rule is a new variant; shipped
//! variants stay as written because existing logic selects them by name.

use alloy_primitives::Address;

use crate::NonZeroAddress;

/// Error returned when an address cannot receive a B-20 balance credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B20CreditRecipientError;

/// An address that can receive a B-20 balance credit.
///
/// Built only through [`B20CreditRecipientStrategy`]. Policy authorization is deliberately outside
/// this type because it is operation-specific and can still reject an otherwise valid credit
/// recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct B20CreditRecipient(NonZeroAddress);

impl B20CreditRecipient {
    /// Returns the wrapped address.
    pub const fn get(self) -> Address {
        self.0.get()
    }
}

/// A frozen rule for choosing a B-20 credit recipient.
///
/// Each variant is a complete predicate. Denim asset and stablecoin logic select
/// [`Self::ExcludingZeroAndSelf`]. A different recipient rule is a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum B20CreditRecipientStrategy {
    /// Rejects the zero address and the token address being credited, so a credit cannot burn by
    /// transfer or sit stranded on the token.
    ExcludingZeroAndSelf,
}

impl B20CreditRecipientStrategy {
    /// Returns a credit recipient under this rule, or [`B20CreditRecipientError`] when `address`
    /// fails it.
    pub fn recipient(
        self,
        address: Address,
        token_address: Address,
    ) -> Result<B20CreditRecipient, B20CreditRecipientError> {
        match self {
            Self::ExcludingZeroAndSelf => {
                if address == Address::ZERO || address == token_address {
                    return Err(B20CreditRecipientError);
                }
                NonZeroAddress::new(address)
                    .map(B20CreditRecipient)
                    .map_err(|_| B20CreditRecipientError)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};

    use crate::{B20CreditRecipientError, B20CreditRecipientStrategy, B20Variant};

    #[test]
    fn excluding_self_accepts_an_ordinary_address() {
        let address = Address::with_last_byte(1);
        assert_eq!(
            B20CreditRecipientStrategy::ExcludingZeroAndSelf
                .recipient(address, Address::with_last_byte(2))
                .unwrap()
                .get(),
            address
        );
    }

    #[test]
    fn excluding_self_rejects_the_zero_address() {
        assert_eq!(
            B20CreditRecipientStrategy::ExcludingZeroAndSelf
                .recipient(Address::ZERO, Address::with_last_byte(1)),
            Err(B20CreditRecipientError)
        );
    }

    #[test]
    fn excluding_self_rejects_the_token_address() {
        let token_address = Address::with_last_byte(1);
        assert_eq!(
            B20CreditRecipientStrategy::ExcludingZeroAndSelf
                .recipient(token_address, token_address),
            Err(B20CreditRecipientError)
        );
    }

    #[test]
    fn excluding_self_accepts_a_different_b20_address() {
        let address = B20Variant::compute_address_for_discriminant(
            Address::repeat_byte(0x11),
            u8::MAX,
            B256::ZERO,
        )
        .0;
        assert_eq!(
            B20CreditRecipientStrategy::ExcludingZeroAndSelf
                .recipient(address, Address::repeat_byte(0x22))
                .unwrap()
                .get(),
            address
        );
    }
}
