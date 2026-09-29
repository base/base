//! Token balance storage layouts.

use alloy_primitives::{Address, U256, keccak256};
use base_execution_txpool::{ValidityOperator, ValidityPredicate};
use serde::{Deserialize, Serialize};

/// Where a token stores holder balances, so the payer can read them and gate
/// inclusion on them without executing the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "layout", rename_all = "snake_case", deny_unknown_fields)]
pub enum BalanceLayout {
    /// Circle `FiatTokenV2_2`, used by USDC, EURC, and cbBTC: balances at
    /// slot 9 with a blacklist flag in the top bit, and a `paused` flag in
    /// slot 1.
    FiatToken,
    /// A plain `mapping(address => uint256)` of balances.
    Mapping {
        /// Storage slot of the mapping.
        slot: U256,
    },
}

impl BalanceLayout {
    /// Slot of the `FiatToken` `balanceAndBlacklistStates` mapping.
    pub const FIAT_TOKEN_BALANCES_SLOT: U256 = U256::from_limbs([9, 0, 0, 0]);

    /// Bit of a `FiatToken` balance word that flags a blacklisted holder.
    pub const FIAT_TOKEN_BLACKLIST_BIT: usize = 255;

    /// Slot packing the `FiatToken` `pauser` address with its `paused` flag.
    pub const FIAT_TOKEN_PAUSED_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

    /// Bit offset of the `FiatToken` `paused` flag, directly above `pauser`.
    pub const FIAT_TOKEN_PAUSED_BIT_OFFSET: usize = 160;

    /// Returns the storage slot holding `holder`'s balance word.
    pub fn slot(&self, holder: Address) -> U256 {
        let mapping = match self {
            Self::FiatToken => Self::FIAT_TOKEN_BALANCES_SLOT,
            Self::Mapping { slot } => *slot,
        };
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(holder.as_slice());
        preimage[32..].copy_from_slice(&mapping.to_be_bytes::<32>());
        keccak256(preimage).into()
    }

    /// Returns the bits of a balance word that hold the balance.
    pub fn balance_mask(&self) -> U256 {
        match self {
            Self::FiatToken => U256::MAX >> 1,
            Self::Mapping { .. } => U256::MAX,
        }
    }

    /// Returns the balance stored in `word`.
    pub fn balance(&self, word: U256) -> U256 {
        word & self.balance_mask()
    }

    /// Whether `word` marks its holder as barred from transferring.
    pub const fn is_blacklisted(&self, word: U256) -> bool {
        matches!(self, Self::FiatToken) && word.bit(Self::FIAT_TOKEN_BLACKLIST_BIT)
    }

    /// Returns predicates that hold while `holder` can transfer `amount` of
    /// `token`.
    pub fn predicates(
        &self,
        token: Address,
        holder: Address,
        amount: U256,
    ) -> Vec<ValidityPredicate> {
        let slot = self.slot(holder);
        let mut predicates = vec![ValidityPredicate::Storage {
            address: token,
            slot,
            mask: self.balance_mask(),
            op: ValidityOperator::GreaterThanOrEqual,
            value: amount,
        }];
        if matches!(self, Self::FiatToken) {
            predicates.push(ValidityPredicate::Storage {
                address: token,
                slot,
                mask: U256::from(1) << Self::FIAT_TOKEN_BLACKLIST_BIT,
                op: ValidityOperator::Equal,
                value: U256::ZERO,
            });
            predicates.push(ValidityPredicate::Storage {
                address: token,
                slot: Self::FIAT_TOKEN_PAUSED_SLOT,
                mask: U256::from(u8::MAX) << Self::FIAT_TOKEN_PAUSED_BIT_OFFSET,
                op: ValidityOperator::Equal,
                value: U256::ZERO,
            });
        }
        predicates
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};

    use super::*;

    const HOLDER: Address = address!("0x3304E22DDaa22bCdC5fCa2269b418046aE7b566A");

    #[test]
    fn fiat_token_slot_matches_mainnet_usdc() {
        // `cast index address 0x3304E22DDaa22bCdC5fCa2269b418046aE7b566A 9`
        assert_eq!(
            BalanceLayout::FiatToken.slot(HOLDER),
            U256::from_be_bytes(
                b256!("0xe2b3b673a055a1e453ea13143ec9fd0b9dbe2b71b7651f18d5a3efc32672b0f9").0
            )
        );
    }

    #[test]
    fn fiat_token_balance_ignores_blacklist_bit() {
        let layout = BalanceLayout::FiatToken;
        let word = (U256::from(1) << BalanceLayout::FIAT_TOKEN_BLACKLIST_BIT) | U256::from(5);
        assert_eq!(layout.balance(word), U256::from(5));
        assert!(layout.is_blacklisted(word));
        assert!(!layout.is_blacklisted(U256::from(5)));
    }

    #[test]
    fn mapping_balance_uses_full_word() {
        let layout = BalanceLayout::Mapping { slot: U256::from(51) };
        assert_eq!(layout.balance(U256::MAX), U256::MAX);
        assert!(!layout.is_blacklisted(U256::MAX));
        assert_eq!(layout.predicates(Address::ZERO, HOLDER, U256::from(1)).len(), 1);
    }

    #[test]
    fn fiat_token_predicates_gate_balance_blacklist_and_pause() {
        let token = Address::repeat_byte(0x11);
        let predicates = BalanceLayout::FiatToken.predicates(token, HOLDER, U256::from(7));
        let slot = BalanceLayout::FiatToken.slot(HOLDER);
        assert_eq!(
            predicates,
            vec![
                ValidityPredicate::Storage {
                    address: token,
                    slot,
                    mask: U256::MAX >> 1,
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::from(7),
                },
                ValidityPredicate::Storage {
                    address: token,
                    slot,
                    mask: U256::from(1) << 255,
                    op: ValidityOperator::Equal,
                    value: U256::ZERO,
                },
                ValidityPredicate::Storage {
                    address: token,
                    slot: U256::from(1),
                    mask: U256::from(0xff) << 160,
                    op: ValidityOperator::Equal,
                    value: U256::ZERO,
                },
            ]
        );
    }

    #[test]
    fn layout_parses_from_operator_config() {
        let fiat: BalanceLayout = toml::from_str(r#"layout = "fiat_token""#).unwrap();
        assert_eq!(fiat, BalanceLayout::FiatToken);
        let mapping: BalanceLayout = toml::from_str("layout = \"mapping\"\nslot = \"51\"").unwrap();
        assert_eq!(mapping, BalanceLayout::Mapping { slot: U256::from(51) });
    }
}
