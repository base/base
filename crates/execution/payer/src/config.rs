//! On-chain payer configuration model.
//!
//! [`PayerConfig`] is the node-side view of the on-chain payer-config contract:
//! the admin EOA that co-signs and receives payment, an enabled flag, and the
//! accepted-token list. Each [`TokenConfig`] carries a [`PriceSource`] that is
//! either a flat rate or an external feed, plus the ERC-8168 `feeRecipient` and
//! payer `margin`. The reader layer decodes the contract's storage into this
//! model each block; this crate then derives rates and payment amounts from it.

use alloy_primitives::{Address, U256};

use crate::{
    error::PricingError,
    feed::{FeedConfig, FeedReading},
    rate::Rate,
};

/// How the price of one accepted token is sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    /// A fixed token-atomic-units-per-native-wei rate, needing no external call.
    Flat(Rate),
    /// A price read from an external oracle contract.
    Feed(FeedConfig),
}

impl PriceSource {
    /// The feed configuration backing this source, or `None` for a flat rate.
    /// The reader layer uses this to decide whether an oracle `STATICCALL` is
    /// needed before pricing.
    pub const fn feed(&self) -> Option<&FeedConfig> {
        match self {
            Self::Flat(_) => None,
            Self::Feed(feed) => Some(feed),
        }
    }

    /// Resolves this source to an exact [`Rate`]. A [`PriceSource::Flat`]
    /// ignores `reading`; a [`PriceSource::Feed`] requires one (supplied by the
    /// reader layer) and enforces its staleness bound against `now`.
    pub fn rate(&self, reading: Option<FeedReading>, now: u64) -> Result<Rate, PricingError> {
        match self {
            Self::Flat(rate) => Ok(*rate),
            Self::Feed(feed) => feed.rate(reading.ok_or(PricingError::MissingReading)?, now),
        }
    }
}

/// Configuration for a single accepted payment token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenConfig {
    /// ERC-20 token contract accepted for gas payment.
    pub token: Address,
    /// Phase-0 transfer destination (ERC-8168 `TokenChoice.feeRecipient`).
    pub fee_recipient: Address,
    /// How this token's price is sourced.
    pub price_source: PriceSource,
    /// Payer margin in basis points, folded into the quoted amount.
    pub margin_bps: u16,
}

impl TokenConfig {
    /// The gross phase-0 `paymentAmount` for this token covering `gas_limit ×
    /// max_fee_per_gas` wei, at the resolved rate plus [`Self::margin_bps`].
    pub fn payment_amount(
        &self,
        reading: Option<FeedReading>,
        now: u64,
        gas_limit: u64,
        max_fee_per_gas: u128,
    ) -> Result<U256, PricingError> {
        self.price_source
            .rate(reading, now)?
            .payment_amount(gas_limit, max_fee_per_gas, self.margin_bps)
    }
}

/// Node-side view of the on-chain payer configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayerConfig {
    /// Admin EOA that co-signs (`payer` field / `payer_auth`) and receives
    /// token payment.
    pub payer: Address,
    /// Whether the payer service is currently accepting transactions.
    pub enabled: bool,
    /// Accepted payment tokens and their terms.
    pub tokens: Vec<TokenConfig>,
}

impl PayerConfig {
    /// Looks up the configuration for `token`, if it is accepted.
    pub fn token(&self, token: Address) -> Option<&TokenConfig> {
        self.tokens.iter().find(|entry| entry.token == token)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;
    use crate::feed::{AnswerShape, FeedDirection};

    fn flat_token() -> TokenConfig {
        TokenConfig {
            token: address!("0x0000000000000000000000000000000000000011"),
            fee_recipient: address!("0x0000000000000000000000000000000000000022"),
            price_source: PriceSource::Flat(Rate::new(U256::from(1u64), U256::from(400_000_000u64))),
            margin_bps: 0,
        }
    }

    #[test]
    fn flat_source_needs_no_reading() {
        let token = flat_token();
        assert!(token.price_source.feed().is_none());
        let amount = token.payment_amount(None, 0, 1_000_000_000, 1_000_000_000).unwrap();
        assert_eq!(amount, U256::from(2_500_000_000u64));
    }

    #[test]
    fn feed_source_requires_reading() {
        let feed = FeedConfig {
            oracle: address!("0x0000000000000000000000000000000000000abc"),
            selector: [0, 0, 0, 0],
            answer_shape: AnswerShape::SingleWord,
            direction: FeedDirection::NativePerToken,
            answer_decimals: 18,
            token_decimals: 6,
            staleness_bound: 0,
        };
        let source = PriceSource::Feed(feed);
        assert!(source.feed().is_some());
        assert_eq!(source.rate(None, 0).unwrap_err(), PricingError::MissingReading);

        let reading = FeedReading { answer: U256::from(400_000_000_000_000u64), updated_at: None };
        assert!(source.rate(Some(reading), 0).is_ok());
    }

    #[test]
    fn token_lookup() {
        let token = flat_token();
        let addr = token.token;
        let config = PayerConfig { payer: address!("0x0000000000000000000000000000000000000099"), enabled: true, tokens: vec![token] };
        assert!(config.token(addr).is_some());
        assert!(config.token(address!("0x00000000000000000000000000000000000000ff")).is_none());
    }
}
