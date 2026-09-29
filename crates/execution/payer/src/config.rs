//! Operator configuration for the payer.

use std::{collections::HashSet, path::Path};

use alloy_primitives::{Address, U256};
use base_common_price_feed::{
    ChainlinkFeed, FeedResolveError, PriceLeg, PricePath, PriceQuote, StateCall, StateCallError,
};
use revm::Database;
use serde::{Deserialize, Serialize};

use crate::{BalanceLayout, IERC20, PayerToken};

/// Error loading or validating a [`PayerConfig`].
#[derive(Debug, thiserror::Error)]
pub enum PayerConfigError {
    /// The configuration file could not be read.
    #[error("failed to read payer config: {0}")]
    Io(#[from] std::io::Error),
    /// The configuration file is not valid TOML for a [`PayerConfig`].
    #[error("failed to parse payer config: {0}")]
    Parse(#[from] toml::de::Error),
    /// `max_expiry_secs` is zero, so no transaction could be co-signed.
    #[error("max_expiry_secs must be positive")]
    ZeroExpiry,
    /// Two tokens share a contract address.
    #[error("token {token} is configured more than once")]
    DuplicateToken {
        /// Repeated token contract.
        token: Address,
    },
    /// An ETH-quoted token has no feeds, so it would be priced as ETH itself.
    #[error("{symbol} is quoted in ETH but has no price legs")]
    EmptyPricePath {
        /// Token symbol.
        symbol: String,
    },
    /// The spread does not cover the deviation its feeds may lag the market by.
    #[error(
        "{symbol} spread of {spread_bps} bps is below its feeds' {deviation_bps} bps deviation"
    )]
    SpreadBelowDeviation {
        /// Token symbol.
        symbol: String,
        /// Configured spread.
        spread_bps: u32,
        /// Summed deviation thresholds of the token's feeds.
        deviation_bps: u32,
    },
    /// The signing key is not the payer account's own key.
    #[error("signer {signer} cannot co-sign for payer {payer}")]
    SignerMismatch {
        /// Configured payer.
        payer: Address,
        /// Signer address.
        signer: Address,
    },
}

/// Error resolving a [`TokenConfig`] against chain state.
#[derive(Debug, thiserror::Error)]
pub enum TokenResolveError<E> {
    /// A feed proxy could not be resolved and verified.
    #[error("failed to resolve feed proxy {proxy}: {source}")]
    Feed {
        /// Feed proxy.
        proxy: Address,
        /// Resolution error.
        source: Box<FeedResolveError<E>>,
    },
    /// `balanceOf` could not be called on the token.
    #[error(transparent)]
    BalanceOf(#[from] StateCallError<E>),
    /// The balance slot could not be read.
    #[error("failed to read balance slot: {0}")]
    Database(E),
    /// The probe holder has no balance, so it cannot verify the layout.
    #[error("probe holder {holder} has no balance")]
    EmptyProbe {
        /// Probe holder.
        holder: Address,
    },
    /// The configured layout does not locate the probe holder's balance.
    #[error("balance layout reads {stored} for {holder}, but balanceOf returns {balance_of}")]
    BalanceLayoutMismatch {
        /// Probe holder.
        holder: Address,
        /// Balance read through the layout.
        stored: U256,
        /// Balance reported by the token.
        balance_of: U256,
    },
}

/// Operator configuration of the payer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayerConfig {
    /// Terms shared by every token.
    pub terms: PayerTerms,
    /// ETH / USD feed converting USD-quoted token prices.
    pub eth_usd: FeedConfig,
    /// Accepted tokens.
    pub tokens: Vec<TokenConfig>,
}

/// Terms the payer offers and enforces for every token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayerTerms {
    /// Payer account, which is also the phase-0 recipient.
    pub payer: Address,
    /// Maximum seconds between co-signing and a transaction's `valid_before`.
    pub max_expiry_secs: u64,
    /// Seconds a quoted rate may be cached.
    pub quote_ttl_secs: u64,
    /// Gas assumed for an intent's calls when `payer_getTerms` omits `gasLimit`.
    pub default_gas_limit: u64,
    /// Maximum `gas_limit` the payer co-signs.
    #[serde(default)]
    pub max_gas_limit: Option<u64>,
    /// Maximum `gas_limit × max_fee_per_gas` the payer co-signs, in wei.
    #[serde(default)]
    pub max_cost_wei: Option<U256>,
}

/// A Chainlink feed proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    /// Feed proxy address.
    pub proxy: Address,
    /// Deviation threshold that triggers a new round, in basis points.
    pub deviation_bps: u32,
}

/// A feed on a token's price path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegConfig {
    /// Feed proxy address.
    pub proxy: Address,
    /// Deviation threshold that triggers a new round, in basis points.
    pub deviation_bps: u32,
    /// Whether the feed is quoted the other way round.
    #[serde(default)]
    pub invert: bool,
}

/// Currency a token's price path ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteConfig {
    /// Priced in USD, then converted through the ETH / USD feed.
    Usd,
    /// Priced directly in ETH.
    Eth,
}

/// Feeds pricing a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceConfig {
    /// Currency the legs price the token in.
    pub quote: QuoteConfig,
    /// Feeds whose product is the token's price.
    pub legs: Vec<LegConfig>,
}

/// A token the payer accepts for gas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    /// Display symbol.
    pub symbol: String,
    /// Token contract.
    pub address: Address,
    /// Token decimals.
    pub decimals: u8,
    /// Markup over the market rate, in basis points. Must cover the summed
    /// deviation of the token's feeds, since a feed may lag the market by that
    /// much without a new round.
    pub spread_bps: u32,
    /// Gas the phase-0 transfer adds to `gas_limit`.
    pub payment_gas: u64,
    /// Holder with a non-zero balance, used to verify [`Self::balance`].
    pub probe_holder: Address,
    /// Feeds pricing the token.
    pub price: PriceConfig,
    /// Where the token stores holder balances.
    pub balance: BalanceLayout,
}

impl TokenConfig {
    /// Returns the summed deviation thresholds of the feeds pricing the token.
    pub fn deviation_bps(&self, eth_usd: &FeedConfig) -> u32 {
        let quote = match self.price.quote {
            QuoteConfig::Usd => eth_usd.deviation_bps,
            QuoteConfig::Eth => 0,
        };
        self.price.legs.iter().fold(quote, |total, leg| total.saturating_add(leg.deviation_bps))
    }

    /// Resolves the token's feeds in `db` and verifies its balance layout
    /// there.
    pub fn resolve<DB: Database>(
        &self,
        db: &mut DB,
        eth_usd: &FeedConfig,
    ) -> Result<PayerToken, TokenResolveError<DB::Error>> {
        let quote = match self.price.quote {
            QuoteConfig::Eth => PriceQuote::Eth,
            QuoteConfig::Usd => PriceQuote::Usd { eth_usd: Self::feed(db, eth_usd.proxy)? },
        };
        let legs = self
            .price
            .legs
            .iter()
            .map(|leg| Ok(PriceLeg { feed: Self::feed(db, leg.proxy)?, invert: leg.invert }))
            .collect::<Result<_, TokenResolveError<DB::Error>>>()?;
        self.verify_balance_layout(db)?;
        Ok(PayerToken {
            symbol: self.symbol.clone(),
            address: self.address,
            decimals: self.decimals,
            spread_bps: self.spread_bps,
            payment_gas: self.payment_gas,
            price: PricePath { quote, legs },
            balance: self.balance,
        })
    }

    fn feed<DB: Database>(
        db: &mut DB,
        proxy: Address,
    ) -> Result<ChainlinkFeed, TokenResolveError<DB::Error>> {
        ChainlinkFeed::resolve(db, proxy)
            .map_err(|source| TokenResolveError::Feed { proxy, source: Box::new(source) })
    }

    /// Checks that the balance read through the layout equals `balanceOf` for
    /// the probe holder.
    fn verify_balance_layout<DB: Database>(
        &self,
        db: &mut DB,
    ) -> Result<(), TokenResolveError<DB::Error>> {
        let holder = self.probe_holder;
        let balance_of =
            StateCall::call(db, self.address, &IERC20::balanceOfCall { account: holder })?;
        if balance_of.is_zero() {
            return Err(TokenResolveError::EmptyProbe { holder });
        }
        let word = db
            .storage(self.address, self.balance.slot(holder))
            .map_err(TokenResolveError::Database)?;
        let stored = self.balance.balance(word);
        if stored != balance_of {
            return Err(TokenResolveError::BalanceLayoutMismatch { holder, stored, balance_of });
        }
        Ok(())
    }
}

impl PayerConfig {
    /// Reads and validates the TOML configuration at `path`.
    pub fn load(path: &Path) -> Result<Self, PayerConfigError> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    /// Checks the configuration without reading chain state.
    pub fn validate(&self) -> Result<(), PayerConfigError> {
        if self.terms.max_expiry_secs == 0 {
            return Err(PayerConfigError::ZeroExpiry);
        }
        let mut addresses = HashSet::new();
        for token in &self.tokens {
            if !addresses.insert(token.address) {
                return Err(PayerConfigError::DuplicateToken { token: token.address });
            }
            if token.price.quote == QuoteConfig::Eth && token.price.legs.is_empty() {
                return Err(PayerConfigError::EmptyPricePath { symbol: token.symbol.clone() });
            }
            let deviation_bps = token.deviation_bps(&self.eth_usd);
            if token.spread_bps < deviation_bps {
                return Err(PayerConfigError::SpreadBelowDeviation {
                    symbol: token.symbol.clone(),
                    spread_bps: token.spread_bps,
                    deviation_bps,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use alloy_sol_types::SolCall;
    use base_common_price_feed::test_utils::{MockFeed, ViewContract};
    use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

    use super::*;

    const CONFIG: &str = r#"
        [terms]
        payer = "0xCcCcCcCcCcCcCcCcCcCcCcCcCcCcCcCcCcCcCcCc"
        max_expiry_secs = 10
        quote_ttl_secs = 15
        default_gas_limit = 100000
        max_cost_wei = "0xB5E620F48000"

        [eth_usd]
        proxy = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70"
        deviation_bps = 15

        [[tokens]]
        symbol = "USDC"
        address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        decimals = 6
        spread_bps = 100
        payment_gas = 25000
        probe_holder = "0x3304E22DDaa22bCdC5fCa2269b418046aE7b566A"
        balance = { layout = "fiat_token" }
        price = { quote = "usd", legs = [
            { proxy = "0x7e860098F58bBFC8648a4311b374B1D669a2bc6B", deviation_bps = 30 },
        ] }

        [[tokens]]
        symbol = "cbETH"
        address = "0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22"
        decimals = 18
        spread_bps = 100
        payment_gas = 30000
        probe_holder = "0x3304E22DDaa22bCdC5fCa2269b418046aE7b566A"
        balance = { layout = "mapping", slot = "51" }
        price = { quote = "eth", legs = [
            { proxy = "0x806b4Ac04501c29769051e42783cF04dCE41440b", deviation_bps = 50 },
        ] }
    "#;

    fn config() -> PayerConfig {
        toml::from_str(CONFIG).unwrap()
    }

    #[test]
    fn parses_operator_config() {
        let config = config();

        assert_eq!(config.terms.max_cost_wei, Some(U256::from(0xB5E6_20F4_8000u64)));
        assert_eq!(config.tokens[0].deviation_bps(&config.eth_usd), 45);
        assert_eq!(config.tokens[1].balance, BalanceLayout::Mapping { slot: U256::from(51) });
        assert_eq!(config.tokens[1].deviation_bps(&config.eth_usd), 50);
        config.validate().unwrap();
    }

    #[test]
    fn rejects_spread_below_feed_deviation() {
        let mut config = config();
        config.tokens[0].spread_bps = 44;

        assert!(matches!(
            config.validate(),
            Err(PayerConfigError::SpreadBelowDeviation { spread_bps: 44, deviation_bps: 45, .. })
        ));
    }

    #[test]
    fn rejects_ambiguous_or_unpriceable_tokens() {
        let mut duplicate = config();
        duplicate.tokens[1].address = duplicate.tokens[0].address;
        assert!(matches!(duplicate.validate(), Err(PayerConfigError::DuplicateToken { .. })));

        let mut unpriced = config();
        unpriced.tokens[1].price.legs.clear();
        assert!(matches!(unpriced.validate(), Err(PayerConfigError::EmptyPricePath { .. })));

        let mut no_expiry = config();
        no_expiry.terms.max_expiry_secs = 0;
        assert!(matches!(no_expiry.validate(), Err(PayerConfigError::ZeroExpiry)));
    }

    fn install(db: &mut InMemoryDB, address: Address, code: Bytes) {
        db.insert_account_info(address, AccountInfo::default().with_code(Bytecode::new_raw(code)));
    }

    fn install_feed(db: &mut InMemoryDB, feed: &MockFeed) {
        install(db, feed.proxy, feed.proxy_code());
        install(db, feed.aggregator, feed.aggregator_code(feed.answer));
        for (slot, value) in feed.aggregator_storage() {
            db.insert_account_storage(feed.aggregator, slot, value).unwrap();
        }
    }

    /// State pricing ETH at $2,000 and USDC at $1, with USDC reporting
    /// `balance_of` for every holder and storing `stored` for the probe holder.
    fn usdc_state(balance_of: u64, stored: U256) -> (InMemoryDB, FeedConfig, TokenConfig) {
        let eth_usd = MockFeed::new(0xe1, 2_000 * 100_000_000);
        let usdc_usd = MockFeed::new(0xe2, 100_000_000);
        let mut token = config().tokens.remove(0);
        token.price.legs[0].proxy = usdc_usd.proxy;

        let mut db = InMemoryDB::default();
        install_feed(&mut db, &eth_usd);
        install_feed(&mut db, &usdc_usd);
        install(
            &mut db,
            token.address,
            ViewContract::new()
                .returns(
                    IERC20::balanceOfCall::SELECTOR,
                    IERC20::balanceOfCall::abi_encode_returns(&U256::from(balance_of)),
                )
                .bytecode(),
        );
        db.insert_account_storage(token.address, token.balance.slot(token.probe_holder), stored)
            .unwrap();
        (db, FeedConfig { proxy: eth_usd.proxy, deviation_bps: 15 }, token)
    }

    #[test]
    fn resolves_feeds_and_verifies_balance_layout() {
        let blacklisted = (U256::from(1) << 255) | U256::from(79_178_602_637u64);
        let (mut db, eth_usd, config) = usdc_state(79_178_602_637, blacklisted);

        let token = config.resolve(&mut db, &eth_usd).unwrap();

        assert_eq!(token.price.quote, PriceQuote::Usd { eth_usd: MockFeed::new(0xe1, 0).feed() });
        assert_eq!(
            token.price.legs,
            vec![PriceLeg { feed: MockFeed::new(0xe2, 0).feed(), invert: false }]
        );
        assert_eq!(token.address, config.address);
    }

    #[test]
    fn rejects_balance_layout_that_misses_the_probe_balance() {
        let (mut db, eth_usd, config) = usdc_state(79_178_602_637, U256::ZERO);

        let error = config.resolve(&mut db, &eth_usd).unwrap_err();

        assert!(matches!(
            error,
            TokenResolveError::BalanceLayoutMismatch { holder, stored, .. }
                if holder == config.probe_holder && stored.is_zero()
        ));
    }

    #[test]
    fn rejects_empty_probe_holder() {
        let (mut db, eth_usd, config) = usdc_state(0, U256::ZERO);

        let error = config.resolve(&mut db, &eth_usd).unwrap_err();

        assert!(matches!(error, TokenResolveError::EmptyProbe { .. }));
    }

    #[test]
    fn rejects_token_whose_feed_is_missing() {
        let (mut db, _, config) = usdc_state(1, U256::from(1));
        let missing = FeedConfig { proxy: Address::repeat_byte(0x99), deviation_bps: 15 };

        let error = config.resolve(&mut db, &missing).unwrap_err();

        assert!(matches!(error, TokenResolveError::Feed { proxy, .. } if proxy == missing.proxy));
    }
}
