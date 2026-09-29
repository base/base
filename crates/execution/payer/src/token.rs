//! Tokens the payer accepts for gas.

use alloy_primitives::Address;
use base_common_price_feed::{PriceError, PricePath, TokenRate};
use revm::Database;

use crate::BalanceLayout;

/// A token whose price feeds and balance layout have been verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayerToken {
    /// Display symbol.
    pub symbol: String,
    /// Token contract.
    pub address: Address,
    /// Token decimals.
    pub decimals: u8,
    /// Markup over the market rate, in basis points.
    pub spread_bps: u32,
    /// Gas the phase-0 transfer adds to `gas_limit`.
    pub payment_gas: u64,
    /// Feeds pricing the token.
    pub price: PricePath,
    /// Where the token stores holder balances.
    pub balance: BalanceLayout,
}

impl PayerToken {
    /// Reads the token's market rate from `db` and applies the spread.
    pub fn quote<DB: Database>(&self, db: &mut DB) -> Result<TokenRate, PriceError<DB::Error>> {
        self.price.rate(db, self.decimals)?.with_spread(self.spread_bps).ok_or(PriceError::Overflow)
    }
}
