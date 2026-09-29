//! Accepted tokens, resolved from configuration against recent state.

use std::sync::Arc;

use alloy_primitives::Address;
use parking_lot::Mutex;
use revm::Database;
use tracing::warn;

use crate::{FeedConfig, PayerToken, TokenConfig};

/// Tokens the payer accepts, re-resolved from their configuration at most
/// once per [`Self::RESOLVE_INTERVAL_SECS`] of chain time.
///
/// Re-resolving follows Chainlink phase upgrades to their new aggregator and
/// picks up tokens whose feeds or probe balance were unavailable before, such
/// as while the node is still syncing. A token that fails to resolve is left
/// out until the next resolution.
#[derive(Debug)]
pub struct TokenBook {
    eth_usd: FeedConfig,
    configs: Vec<TokenConfig>,
    resolved: Mutex<Option<ResolvedTokens>>,
}

/// Tokens resolved at one block.
#[derive(Debug, Clone)]
pub struct ResolvedTokens {
    /// Timestamp, in seconds, of the block the tokens were resolved at.
    pub timestamp: u64,
    /// Tokens that resolved.
    pub tokens: Arc<[PayerToken]>,
}

impl TokenBook {
    /// Chain time, in seconds, a resolution is reused for.
    pub const RESOLVE_INTERVAL_SECS: u64 = 300;

    /// Creates a book that resolves `configs` on first use.
    pub const fn new(eth_usd: FeedConfig, configs: Vec<TokenConfig>) -> Self {
        Self { eth_usd, configs, resolved: Mutex::new(None) }
    }

    /// Whether `token` is configured, whether or not it currently resolves.
    pub fn is_configured(&self, token: Address) -> bool {
        self.configs.iter().any(|config| config.address == token)
    }

    /// Whether no token is configured.
    pub const fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }

    /// Returns the resolved tokens for the block at `timestamp` whose state is
    /// `db`, resolving them again once the last resolution is stale.
    pub fn tokens<DB: Database>(&self, db: &mut DB, timestamp: u64) -> Arc<[PayerToken]> {
        let mut resolved = self.resolved.lock();
        if let Some(current) = resolved.as_ref()
            && current.timestamp <= timestamp
            && timestamp - current.timestamp < Self::RESOLVE_INTERVAL_SECS
        {
            return Arc::clone(&current.tokens);
        }
        let tokens: Arc<[PayerToken]> = self
            .configs
            .iter()
            .filter_map(|config| match config.resolve(db, &self.eth_usd) {
                Ok(token) => Some(token),
                Err(error) => {
                    warn!(token = %config.symbol, error = %error, "failed to resolve payer token");
                    None
                }
            })
            .collect();
        *resolved = Some(ResolvedTokens { timestamp, tokens: Arc::clone(&tokens) });
        tokens
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256};
    use alloy_sol_types::SolCall;
    use base_common_price_feed::test_utils::{MockFeed, ViewContract};
    use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

    use super::*;
    use crate::{BalanceLayout, IERC20, LegConfig, PriceConfig, QuoteConfig};

    const TOKEN: Address = Address::repeat_byte(0x83);
    const PROBE: Address = Address::repeat_byte(0x70);

    fn install(db: &mut InMemoryDB, address: Address, code: Bytes) {
        db.insert_account_info(address, AccountInfo::default().with_code(Bytecode::new_raw(code)));
    }

    fn book(feed: &MockFeed) -> TokenBook {
        TokenBook::new(
            FeedConfig { proxy: feed.proxy, deviation_bps: 0 },
            vec![TokenConfig {
                symbol: "cbETH".to_owned(),
                address: TOKEN,
                decimals: 18,
                spread_bps: 0,
                payment_gas: 30_000,
                probe_holder: PROBE,
                price: PriceConfig {
                    quote: QuoteConfig::Eth,
                    legs: vec![LegConfig { proxy: feed.proxy, deviation_bps: 0, invert: false }],
                },
                balance: BalanceLayout::Mapping { slot: U256::ZERO },
            }],
        )
    }

    /// State holding the token but not yet its feed.
    fn token_db() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        install(
            &mut db,
            TOKEN,
            ViewContract::new()
                .returns(
                    IERC20::balanceOfCall::SELECTOR,
                    IERC20::balanceOfCall::abi_encode_returns(&U256::from(5)),
                )
                .bytecode(),
        );
        let slot = BalanceLayout::Mapping { slot: U256::ZERO }.slot(PROBE);
        db.insert_account_storage(TOKEN, slot, U256::from(5)).unwrap();
        db
    }

    fn add_feed(db: &mut InMemoryDB, feed: &MockFeed) {
        install(db, feed.proxy, feed.proxy_code());
        install(db, feed.aggregator, feed.aggregator_code(feed.answer));
        for (slot, value) in feed.aggregator_storage() {
            db.insert_account_storage(feed.aggregator, slot, value).unwrap();
        }
    }

    #[test]
    fn picks_up_a_feed_once_the_resolution_is_stale() {
        let feed = MockFeed::new(0xa0, 1_100_000_000_000_000_000);
        let book = book(&feed);
        let mut db = token_db();

        assert!(book.tokens(&mut db, 1_000).is_empty());
        add_feed(&mut db, &feed);
        assert!(book.tokens(&mut db, 1_000 + TokenBook::RESOLVE_INTERVAL_SECS - 1).is_empty());

        let tokens = book.tokens(&mut db, 1_000 + TokenBook::RESOLVE_INTERVAL_SECS);

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].address, TOKEN);
        assert_eq!(tokens[0].price.legs[0].feed, feed.feed());
    }

    #[test]
    fn follows_the_proxy_to_a_new_aggregator() {
        let feed = MockFeed::new(0xa0, 1_100_000_000_000_000_000);
        let book = book(&feed);
        let mut db = token_db();
        add_feed(&mut db, &feed);
        assert_eq!(book.tokens(&mut db, 1_000)[0].price.legs[0].feed, feed.feed());

        let upgraded = MockFeed { aggregator: Address::repeat_byte(0xb0), ..feed };
        add_feed(&mut db, &upgraded);
        let tokens = book.tokens(&mut db, 1_000 + TokenBook::RESOLVE_INTERVAL_SECS);

        assert_eq!(tokens[0].price.legs[0].feed, upgraded.feed());
    }

    #[test]
    fn resolves_again_after_a_reorg_to_an_earlier_block() {
        let feed = MockFeed::new(0xa0, 1_100_000_000_000_000_000);
        let book = book(&feed);
        let mut db = token_db();
        assert!(book.tokens(&mut db, 1_000).is_empty());
        add_feed(&mut db, &feed);

        assert_eq!(book.tokens(&mut db, 999).len(), 1);
    }
}
