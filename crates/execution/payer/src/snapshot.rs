//! Per-block resolved price snapshot and ERC-20 balance-slot helpers.
//!
//! The builder resolves every accepted token's [`Rate`] once per block (a
//! single `SLOAD` per slot-backed token — see the `storage` feature's reader),
//! producing a [`PriceSnapshot`]. Per-transaction admission is then
//! chain-read-free for pricing: [`TokenPrice::payment_amount`] derives the exact
//! phase-0 amount from the cached rate, and the sender's ability to pay is a
//! single balance `SLOAD` at [`Erc20::balance_slot`]. The authoritative check
//! remains the phase-0 transfer itself: if it reverts at inclusion the builder
//! discards the transaction as insufficient payment.

use alloy_primitives::{Address, U256, keccak256};

use crate::{error::PricingError, rate::Rate};

/// A single accepted token's resolved price at a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenPrice {
    /// ERC-20 token accepted for gas payment.
    pub token: Address,
    /// Phase-0 transfer destination (ERC-8168 `feeRecipient`).
    pub fee_recipient: Address,
    /// Resolved token-atomic-units-per-native-wei rate at the snapshot block.
    pub rate: Rate,
    /// Payer margin in basis points, folded into the quoted amount.
    pub margin_bps: u16,
}

impl TokenPrice {
    /// The gross phase-0 `paymentAmount` covering `gas_limit × max_fee_per_gas`
    /// wei at the cached rate plus [`Self::margin_bps`] — no chain read.
    pub fn payment_amount(
        &self,
        gas_limit: u64,
        max_fee_per_gas: u128,
    ) -> Result<U256, PricingError> {
        self.rate.payment_amount(gas_limit, max_fee_per_gas, self.margin_bps)
    }
}

/// Payer pricing resolved for one block: the co-signing payer, whether the
/// service is live, and the price of every currently-quotable token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceSnapshot {
    /// Admin EOA that co-signs (`payer` / `payer_auth`) and receives payment.
    pub payer: Address,
    /// Whether the payer service is currently accepting transactions.
    pub enabled: bool,
    /// Resolved prices for the tokens quotable at this block. Tokens whose price
    /// could not be resolved (e.g. a stale oracle) are omitted.
    pub prices: Vec<TokenPrice>,
}

impl PriceSnapshot {
    /// The resolved price for `token`, if it is currently quotable.
    pub fn token(&self, token: Address) -> Option<&TokenPrice> {
        self.prices.iter().find(|price| price.token == token)
    }
}

/// Storage-slot helpers for standard (Solidity-`mapping`) ERC-20 tokens.
#[derive(Debug)]
pub struct Erc20;

impl Erc20 {
    /// Storage slot of `balanceOf[holder]` for a token whose `balances` mapping
    /// is declared at `balances_base_slot`, i.e. Solidity
    /// `keccak256(abi.encode(holder, balances_base_slot))`.
    ///
    /// The base slot is token-specific (well-known for the tokens the payer
    /// accepts); the builder reads the holder's balance with one `SLOAD` at this
    /// slot to pre-screen payment before running the phase-0 transfer.
    pub fn balance_slot(holder: Address, balances_base_slot: U256) -> U256 {
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(holder.as_slice());
        buf[32..64].copy_from_slice(&balances_base_slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn token_price_payment_amount_uses_cached_rate() {
        let price = TokenPrice {
            token: address!("0x0000000000000000000000000000000000000011"),
            fee_recipient: address!("0x0000000000000000000000000000000000000022"),
            rate: Rate::new(U256::from(1u64), U256::from(400_000_000u64)),
            margin_bps: 0,
        };
        assert_eq!(
            price.payment_amount(1_000_000_000, 1_000_000_000).unwrap(),
            U256::from(2_500_000_000u64)
        );
    }

    #[test]
    fn snapshot_lookup() {
        let price = TokenPrice {
            token: address!("0x0000000000000000000000000000000000000011"),
            fee_recipient: address!("0x0000000000000000000000000000000000000022"),
            rate: Rate::new(U256::from(1u64), U256::from(2u64)),
            margin_bps: 0,
        };
        let snapshot = PriceSnapshot {
            payer: address!("0x0000000000000000000000000000000000000099"),
            enabled: true,
            prices: vec![price],
        };
        assert_eq!(snapshot.token(price.token), Some(&price));
        assert!(snapshot.token(address!("0x00000000000000000000000000000000000000ff")).is_none());
    }

    #[test]
    fn balance_slot_matches_solidity_mapping_encoding() {
        let holder = address!("0x1234567890abcDEF1234567890aBcdef12345678");
        let base = U256::from(9u64);
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(holder.as_slice());
        buf[32..64].copy_from_slice(&base.to_be_bytes::<32>());
        assert_eq!(Erc20::balance_slot(holder, base), U256::from_be_bytes(keccak256(buf).0));
    }
}
