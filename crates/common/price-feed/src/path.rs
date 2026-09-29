//! Token prices composed from one or more Chainlink feeds.

use alloy_primitives::{U256, U512};
use revm::Database;

use crate::{ChainlinkFeed, FeedReadError, TokenRate};

/// Error pricing a token through a [`PricePath`].
#[derive(Debug, thiserror::Error)]
pub enum PriceError<E> {
    /// A feed on the path could not be read.
    #[error(transparent)]
    Feed(#[from] FeedReadError<E>),
    /// The rate does not fit in 256 bits.
    #[error("token rate overflows")]
    Overflow,
    /// The token is worth more than one atomic unit per ETH, so no rate can
    /// charge for gas in it.
    #[error("token rate rounds to zero")]
    ZeroRate,
}

/// One feed on a [`PricePath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PriceLeg {
    /// Feed supplying the conversion.
    pub feed: ChainlinkFeed,
    /// Whether the feed is quoted the other way round, such as `USD / ARS` on
    /// a path pricing an ARS token in USD.
    pub invert: bool,
}

/// Currency a [`PricePath`] prices its token in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriceQuote {
    /// The path prices the token in USD; `eth_usd` converts ETH to USD.
    Usd {
        /// ETH / USD feed.
        eth_usd: ChainlinkFeed,
    },
    /// The path prices the token directly in ETH.
    Eth,
}

/// A token price composed by multiplying the answers of its [`PriceLeg`]s.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PricePath {
    /// Currency the legs price the token in.
    pub quote: PriceQuote,
    /// Feeds whose product is the token's price in [`Self::quote`].
    pub legs: Vec<PriceLeg>,
}

impl PricePath {
    /// Reads every feed on the path from `db` and returns the market rate for a
    /// token with `token_decimals` decimals, rounded down.
    pub fn rate<DB: Database>(
        &self,
        db: &mut DB,
        token_decimals: u8,
    ) -> Result<TokenRate, PriceError<DB::Error>> {
        // rate = 10^token_decimals * (ETH price in quote) / (token price in quote)
        let mut numerator = U512::from(10).pow(U512::from(token_decimals));
        let mut denominator = U512::from(1);
        if let PriceQuote::Usd { eth_usd } = self.quote {
            let answer = U512::from(eth_usd.read(db)?.answer);
            numerator = numerator.checked_mul(answer).ok_or(PriceError::Overflow)?;
            denominator =
                denominator.checked_mul(Self::scale(eth_usd)).ok_or(PriceError::Overflow)?;
        }
        for leg in &self.legs {
            let answer = U512::from(leg.feed.read(db)?.answer);
            let (to_numerator, to_denominator) = if leg.invert {
                (answer, Self::scale(leg.feed))
            } else {
                (Self::scale(leg.feed), answer)
            };
            numerator = numerator.checked_mul(to_numerator).ok_or(PriceError::Overflow)?;
            denominator = denominator.checked_mul(to_denominator).ok_or(PriceError::Overflow)?;
        }
        let rate = U256::checked_from_limbs_slice((numerator / denominator).as_limbs())
            .ok_or(PriceError::Overflow)?;
        if rate.is_zero() {
            return Err(PriceError::ZeroRate);
        }
        Ok(TokenRate(rate))
    }

    fn scale(feed: ChainlinkFeed) -> U512 {
        U512::from(10).pow(U512::from(feed.decimals))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use revm::database::InMemoryDB;

    use super::*;
    use crate::ChainlinkLayout;

    /// Writes a single round with `answer` for a new aggregator at `byte`.
    fn feed(db: &mut InMemoryDB, byte: u8, decimals: u8, answer: u128) -> ChainlinkFeed {
        let feed = ChainlinkFeed {
            aggregator: Address::repeat_byte(byte),
            layout: ChainlinkLayout::Ocr2,
            decimals,
        };
        let round_id = 1u32;
        let hot_vars = U256::from(round_id) << ChainlinkLayout::ROUND_ID_BIT_OFFSET;
        db.insert_account_storage(feed.aggregator, feed.layout.hot_vars_slot(), hot_vars).unwrap();
        db.insert_account_storage(
            feed.aggregator,
            feed.layout.transmission_slot(round_id),
            U256::from(answer),
        )
        .unwrap();
        feed
    }

    #[test]
    fn prices_usdc_through_usd() {
        let mut db = InMemoryDB::default();
        let eth_usd = feed(&mut db, 1, 8, 2_000 * 100_000_000);
        let usdc_usd = feed(&mut db, 2, 8, 100_000_000);
        let path = PricePath {
            quote: PriceQuote::Usd { eth_usd },
            legs: vec![PriceLeg { feed: usdc_usd, invert: false }],
        };

        // ERC-8168: 2,000 USDC per ETH is `rate` 0x77359400.
        assert_eq!(path.rate(&mut db, 6).unwrap(), TokenRate(U256::from(0x7735_9400u64)));
    }

    #[test]
    fn prices_inverted_fiat_feed() {
        let mut db = InMemoryDB::default();
        let eth_usd = feed(&mut db, 1, 8, 2_000 * 100_000_000);
        let usd_ars = feed(&mut db, 2, 8, 1_000 * 100_000_000);
        let path = PricePath {
            quote: PriceQuote::Usd { eth_usd },
            legs: vec![PriceLeg { feed: usd_ars, invert: true }],
        };

        // 2,000 USD per ETH at 1,000 ARS per USD is 2,000,000 ARS per ETH.
        let expected = U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(18));
        assert_eq!(path.rate(&mut db, 18).unwrap(), TokenRate(expected));
    }

    #[test]
    fn prices_directly_in_eth() {
        let mut db = InMemoryDB::default();
        let cbeth_eth = feed(&mut db, 1, 18, 1_140_000_000_000_000_000);
        let path = PricePath {
            quote: PriceQuote::Eth,
            legs: vec![PriceLeg { feed: cbeth_eth, invert: false }],
        };

        // 1 / 1.14 cbETH per ETH, rounded down.
        assert_eq!(
            path.rate(&mut db, 18).unwrap(),
            TokenRate(U256::from(877_192_982_456_140_350u64))
        );
    }

    #[test]
    fn multiplies_every_leg() {
        let mut db = InMemoryDB::default();
        let eth_usd = feed(&mut db, 1, 8, 2_000 * 100_000_000);
        let lbtc_btc = feed(&mut db, 2, 8, 99_000_000);
        let btc_usd = feed(&mut db, 3, 8, 100_000 * 100_000_000);
        let path = PricePath {
            quote: PriceQuote::Usd { eth_usd },
            legs: vec![
                PriceLeg { feed: lbtc_btc, invert: false },
                PriceLeg { feed: btc_usd, invert: false },
            ],
        };

        // 2,000 / (0.99 * 100,000) LBTC per ETH at 8 decimals, rounded down.
        assert_eq!(path.rate(&mut db, 8).unwrap(), TokenRate(U256::from(2_020_202u64)));
    }

    #[test]
    fn rejects_rate_that_rounds_to_zero() {
        let mut db = InMemoryDB::default();
        let eth_usd = feed(&mut db, 1, 8, 2_000 * 100_000_000);
        let pricey_usd = feed(&mut db, 2, 8, 10_000 * 100_000_000);
        let path = PricePath {
            quote: PriceQuote::Usd { eth_usd },
            legs: vec![PriceLeg { feed: pricey_usd, invert: false }],
        };

        assert!(matches!(path.rate(&mut db, 0), Err(PriceError::ZeroRate)));
    }

    #[test]
    fn surfaces_unreadable_feed() {
        let mut db = InMemoryDB::default();
        let eth_usd = feed(&mut db, 1, 8, 2_000 * 100_000_000);
        let missing = ChainlinkFeed {
            aggregator: Address::repeat_byte(9),
            layout: ChainlinkLayout::Dual,
            decimals: 8,
        };
        let path = PricePath {
            quote: PriceQuote::Usd { eth_usd },
            legs: vec![PriceLeg { feed: missing, invert: false }],
        };

        assert!(matches!(path.rate(&mut db, 6), Err(PriceError::Feed(_))));
    }
}
