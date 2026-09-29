//! Operator configuration for the payer.

use std::collections::HashSet;

use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockId, TransactionInput, TransactionRequest};
use alloy_sol_types::SolCall;
use alloy_transport::TransportError;
use base_common_price_feed::{ChainlinkFeed, FeedResolveError, PriceLeg, PricePath, PriceQuote};
use serde::{Deserialize, Serialize};

use crate::{BalanceLayout, IERC20, PayerToken};

/// Error validating or resolving a [`PayerConfig`].
#[derive(Debug, thiserror::Error)]
pub enum PayerConfigError {
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
    /// A feed proxy could not be resolved and verified.
    #[error("failed to resolve feed proxy {proxy}: {source}")]
    Feed {
        /// Feed proxy.
        proxy: Address,
        /// Resolution error.
        source: Box<FeedResolveError>,
    },
    /// An RPC request verifying a balance layout failed.
    #[error("balance layout RPC request failed: {0}")]
    Transport(#[from] TransportError),
    /// `balanceOf` returned data that does not decode.
    #[error("failed to decode {symbol} balanceOf output: {source}")]
    Decode {
        /// Token symbol.
        symbol: String,
        /// Decoding error.
        source: alloy_sol_types::Error,
    },
    /// The probe holder has no balance, so it cannot verify the layout.
    #[error("{symbol} probe holder {holder} has no balance")]
    EmptyProbe {
        /// Token symbol.
        symbol: String,
        /// Probe holder.
        holder: Address,
    },
    /// The configured layout does not locate the probe holder's balance.
    #[error(
        "{symbol} balance layout reads {stored} for {holder}, but balanceOf returns {balance_of}"
    )]
    BalanceLayoutMismatch {
        /// Token symbol.
        symbol: String,
        /// Probe holder.
        holder: Address,
        /// Balance read through the layout.
        stored: U256,
        /// Balance reported by the token.
        balance_of: U256,
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
}

impl PayerConfig {
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

    /// Validates the configuration, then resolves every feed and verifies every
    /// balance layout against `provider`.
    pub async fn resolve<P: Provider>(
        &self,
        provider: &P,
    ) -> Result<Vec<PayerToken>, PayerConfigError> {
        self.validate()?;
        let mut eth_usd = None;
        let mut tokens = Vec::with_capacity(self.tokens.len());
        for token in &self.tokens {
            let quote = match token.price.quote {
                QuoteConfig::Eth => PriceQuote::Eth,
                QuoteConfig::Usd => {
                    let feed = match eth_usd {
                        Some(feed) => feed,
                        None => *eth_usd.insert(Self::feed(provider, self.eth_usd.proxy).await?),
                    };
                    PriceQuote::Usd { eth_usd: feed }
                }
            };
            let mut legs = Vec::with_capacity(token.price.legs.len());
            for leg in &token.price.legs {
                legs.push(PriceLeg {
                    feed: Self::feed(provider, leg.proxy).await?,
                    invert: leg.invert,
                });
            }
            Self::verify_balance_layout(provider, token).await?;
            tokens.push(PayerToken {
                symbol: token.symbol.clone(),
                address: token.address,
                decimals: token.decimals,
                spread_bps: token.spread_bps,
                payment_gas: token.payment_gas,
                price: PricePath { quote, legs },
                balance: token.balance,
            });
        }
        Ok(tokens)
    }

    async fn feed<P: Provider>(
        provider: &P,
        proxy: Address,
    ) -> Result<ChainlinkFeed, PayerConfigError> {
        ChainlinkFeed::resolve(provider, proxy)
            .await
            .map_err(|source| PayerConfigError::Feed { proxy, source: Box::new(source) })
    }

    /// Checks that the balance read through the layout equals `balanceOf` for
    /// the token's probe holder, both at the same block.
    async fn verify_balance_layout<P: Provider>(
        provider: &P,
        token: &TokenConfig,
    ) -> Result<(), PayerConfigError> {
        let block = BlockId::number(provider.get_block_number().await?);
        let holder = token.probe_holder;
        let request = TransactionRequest::default().to(token.address).input(TransactionInput::new(
            IERC20::balanceOfCall { account: holder }.abi_encode().into(),
        ));
        let output = provider.call(request).block(block).await?;
        let balance_of = IERC20::balanceOfCall::abi_decode_returns(&output)
            .map_err(|source| PayerConfigError::Decode { symbol: token.symbol.clone(), source })?;
        if balance_of.is_zero() {
            return Err(PayerConfigError::EmptyProbe { symbol: token.symbol.clone(), holder });
        }
        let word = provider
            .get_storage_at(token.address, token.balance.slot(holder))
            .block_id(block)
            .await?;
        let stored = token.balance.balance(word);
        if stored != balance_of {
            return Err(PayerConfigError::BalanceLayoutMismatch {
                symbol: token.symbol.clone(),
                holder,
                stored,
                balance_of,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U64};
    use alloy_provider::ProviderBuilder;
    use alloy_transport::mock::Asserter;

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

    /// Queues the responses verifying the balance layout of `config().tokens[0]`.
    fn probe_responses(balance_of: u64, stored: U256) -> Asserter {
        let asserter = Asserter::new();
        asserter.push_success(&U64::from(51_951_686u64));
        asserter.push_success(&Bytes::from(IERC20::balanceOfCall::abi_encode_returns(
            &U256::from(balance_of),
        )));
        asserter.push_success(&stored);
        asserter
    }

    #[tokio::test]
    async fn verifies_balance_layout_against_balance_of() {
        let token = &config().tokens[0];
        let blacklisted = (U256::from(1) << 255) | U256::from(79_178_602_637u64);
        let provider = ProviderBuilder::new()
            .connect_mocked_client(probe_responses(79_178_602_637, blacklisted));

        PayerConfig::verify_balance_layout(&provider, token).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_balance_layout_that_misses_the_probe_balance() {
        let token = &config().tokens[0];
        let provider = ProviderBuilder::new()
            .connect_mocked_client(probe_responses(79_178_602_637, U256::ZERO));

        let error = PayerConfig::verify_balance_layout(&provider, token).await.unwrap_err();

        assert!(matches!(
            error,
            PayerConfigError::BalanceLayoutMismatch { holder, stored, .. }
                if holder == token.probe_holder && stored.is_zero()
        ));
    }

    #[tokio::test]
    async fn rejects_empty_probe_holder() {
        let token = &config().tokens[0];
        let provider = ProviderBuilder::new().connect_mocked_client(probe_responses(0, U256::ZERO));

        let error = PayerConfig::verify_balance_layout(&provider, token).await.unwrap_err();

        assert!(matches!(error, PayerConfigError::EmptyProbe { .. }));
    }
}
