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
    /// slot 9 sharing their word with a blacklist flag in the top bit.
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

    /// Returns a predicate that holds while `holder` has at least `amount` of
    /// `token`.
    pub fn predicate(&self, token: Address, holder: Address, amount: U256) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address: token,
            slot: self.slot(holder),
            mask: self.balance_mask(),
            op: ValidityOperator::GreaterThanOrEqual,
            value: amount,
        }
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
        let word = (U256::from(1) << 255) | U256::from(5);
        assert_eq!(BalanceLayout::FiatToken.balance(word), U256::from(5));
        assert_eq!(BalanceLayout::Mapping { slot: U256::from(51) }.balance(word), word);
    }

    #[test]
    fn predicate_gates_masked_balance() {
        let token = Address::repeat_byte(0x11);
        assert_eq!(
            BalanceLayout::FiatToken.predicate(token, HOLDER, U256::from(7)),
            ValidityPredicate::Storage {
                address: token,
                slot: BalanceLayout::FiatToken.slot(HOLDER),
                mask: U256::MAX >> 1,
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(7),
            }
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
