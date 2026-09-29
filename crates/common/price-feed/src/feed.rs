//! Chainlink feed reads from aggregator storage.

use alloy_primitives::{Address, I256, U256, aliases::U80};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockId, TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use alloy_transport::TransportError;
use revm::Database;

use crate::ChainlinkLayout;

sol! {
    /// Chainlink `EACAggregatorProxy` reads used to resolve a feed.
    interface IChainlinkProxy {
        /// Returns the aggregator the proxy currently delegates to.
        function aggregator() external view returns (address);

        /// Returns the number of decimals in the feed's answer.
        function decimals() external view returns (uint8);
    }

    /// Chainlink aggregator reads used to verify a storage layout.
    interface IChainlinkAggregator {
        /// Returns the aggregator implementation name and version.
        function typeAndVersion() external pure returns (string memory);

        /// Returns the latest round as reported by the aggregator itself.
        function latestRoundData()
            external
            view
            returns (
                uint80 roundId,
                int256 answer,
                uint256 startedAt,
                uint256 updatedAt,
                uint80 answeredInRound
            );
    }
}

/// Reason a feed's latest round cannot be used as a price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidAnswer {
    /// The aggregator has never transmitted a round.
    #[error("aggregator has no rounds")]
    NoRounds,
    /// The latest round's answer is zero or negative.
    #[error("round {round_id} has a non-positive answer")]
    NonPositive {
        /// Aggregator round id of the rejected answer.
        round_id: u32,
    },
}

/// Error reading a feed's latest answer from state.
#[derive(Debug, thiserror::Error)]
pub enum FeedReadError<E> {
    /// Aggregator storage could not be read.
    #[error("failed to read aggregator storage: {0}")]
    Database(E),
    /// The latest round cannot be used as a price.
    #[error("aggregator {aggregator} has no usable answer: {reason}")]
    InvalidAnswer {
        /// Aggregator whose round was rejected.
        aggregator: Address,
        /// Why the round was rejected.
        reason: InvalidAnswer,
    },
}

/// Error resolving a feed proxy to a verified [`ChainlinkFeed`].
#[derive(Debug, thiserror::Error)]
pub enum FeedResolveError {
    /// An RPC request failed.
    #[error("feed RPC request failed: {0}")]
    Transport(#[from] TransportError),
    /// A contract returned data that does not decode as the expected type.
    #[error("failed to decode call output from {to}: {source}")]
    Decode {
        /// Contract that returned the data.
        to: Address,
        /// Decoding error.
        source: alloy_sol_types::Error,
    },
    /// The proxy delegates to an aggregator whose storage layout is unknown.
    #[error("proxy {proxy} delegates to unsupported aggregator {aggregator} ({type_and_version})")]
    UnsupportedAggregator {
        /// Feed proxy being resolved.
        proxy: Address,
        /// Aggregator the proxy delegates to.
        aggregator: Address,
        /// `typeAndVersion()` reported by the aggregator.
        type_and_version: String,
    },
    /// The latest round cannot be used as a price.
    #[error("aggregator {aggregator} has no usable answer: {reason}")]
    InvalidAnswer {
        /// Aggregator whose round was rejected.
        aggregator: Address,
        /// Why the round was rejected.
        reason: InvalidAnswer,
    },
    /// Reading storage with the selected layout disagrees with `latestRoundData()`.
    #[error("storage of aggregator {aggregator} does not match latestRoundData() under {layout:?}")]
    LayoutMismatch {
        /// Aggregator whose storage disagreed.
        aggregator: Address,
        /// Layout that produced the mismatching read.
        layout: ChainlinkLayout,
    },
}

/// Latest round of a Chainlink feed, decoded from aggregator storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedAnswer {
    /// Aggregator round id.
    pub round_id: u32,
    /// Positive answer, scaled by the feed's decimals.
    pub answer: U256,
    /// Unix timestamp, in seconds, at which the round was transmitted onchain.
    pub updated_at: u64,
}

impl FeedAnswer {
    /// Width of the signed `answer` field at the bottom of a `Transmission` word.
    pub const ANSWER_BITS: usize = 192;

    /// Bit offset of `uint32 transmissionTimestamp` in a `Transmission` word.
    pub const UPDATED_AT_BIT_OFFSET: usize = 224;

    /// Decodes the `Transmission` word of round `round_id`.
    pub fn decode(round_id: u32, transmission: U256) -> Result<Self, InvalidAnswer> {
        if round_id == 0 {
            return Err(InvalidAnswer::NoRounds);
        }
        let answer = transmission & ((U256::from(1) << Self::ANSWER_BITS) - U256::from(1));
        if answer.is_zero() || answer.bit(Self::ANSWER_BITS - 1) {
            return Err(InvalidAnswer::NonPositive { round_id });
        }
        let updated_at = (transmission >> Self::UPDATED_AT_BIT_OFFSET).as_limbs()[0];
        Ok(Self { round_id, answer, updated_at })
    }

    /// Whether this answer is the round `latestRoundData()` reports.
    pub fn matches(&self, round: &IChainlinkAggregator::latestRoundDataReturn) -> bool {
        round.roundId == U80::from(self.round_id)
            && round.answer == I256::from_raw(self.answer)
            && round.updatedAt == U256::from(self.updated_at)
    }
}

/// A Chainlink feed whose aggregator storage layout has been verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainlinkFeed {
    /// Aggregator holding the feed's rounds.
    pub aggregator: Address,
    /// Storage layout of the aggregator.
    pub layout: ChainlinkLayout,
    /// Number of decimals in the feed's answer.
    pub decimals: u8,
}

impl ChainlinkFeed {
    /// Reads the latest answer from aggregator storage in `db`.
    pub fn read<DB: Database>(&self, db: &mut DB) -> Result<FeedAnswer, FeedReadError<DB::Error>> {
        let hot_vars = db
            .storage(self.aggregator, self.layout.hot_vars_slot())
            .map_err(FeedReadError::Database)?;
        let round_id = ChainlinkLayout::latest_round_id(hot_vars);
        let transmission = db
            .storage(self.aggregator, self.layout.transmission_slot(round_id))
            .map_err(FeedReadError::Database)?;
        FeedAnswer::decode(round_id, transmission)
            .map_err(|reason| FeedReadError::InvalidAnswer { aggregator: self.aggregator, reason })
    }

    /// Resolves `proxy` to its current aggregator and verifies the aggregator's
    /// storage layout.
    ///
    /// Every read is pinned to the provider's latest block, and the feed is
    /// accepted only if the storage read equals `latestRoundData()` there.
    pub async fn resolve<P: Provider>(
        provider: &P,
        proxy: Address,
    ) -> Result<Self, FeedResolveError> {
        let block = BlockId::number(provider.get_block_number().await?);
        let aggregator =
            Self::call(provider, proxy, IChainlinkProxy::aggregatorCall {}, block).await?;
        let decimals = Self::call(provider, proxy, IChainlinkProxy::decimalsCall {}, block).await?;
        let type_and_version =
            Self::call(provider, aggregator, IChainlinkAggregator::typeAndVersionCall {}, block)
                .await?;
        let Some(layout) = ChainlinkLayout::from_type_and_version(&type_and_version) else {
            return Err(FeedResolveError::UnsupportedAggregator {
                proxy,
                aggregator,
                type_and_version,
            });
        };

        let hot_vars =
            provider.get_storage_at(aggregator, layout.hot_vars_slot()).block_id(block).await?;
        let round_id = ChainlinkLayout::latest_round_id(hot_vars);
        let transmission = provider
            .get_storage_at(aggregator, layout.transmission_slot(round_id))
            .block_id(block)
            .await?;
        let answer = FeedAnswer::decode(round_id, transmission)
            .map_err(|reason| FeedResolveError::InvalidAnswer { aggregator, reason })?;

        let latest =
            Self::call(provider, aggregator, IChainlinkAggregator::latestRoundDataCall {}, block)
                .await?;
        if !answer.matches(&latest) {
            return Err(FeedResolveError::LayoutMismatch { aggregator, layout });
        }
        Ok(Self { aggregator, layout, decimals })
    }

    async fn call<P: Provider, C: SolCall>(
        provider: &P,
        to: Address,
        call: C,
        block: BlockId,
    ) -> Result<C::Return, FeedResolveError> {
        let request = TransactionRequest::default()
            .to(to)
            .input(TransactionInput::new(call.abi_encode().into()));
        let output = provider.call(request).block(block).await?;
        C::abi_decode_returns(&output).map_err(|source| FeedResolveError::Decode { to, source })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U64, address, uint};
    use alloy_provider::ProviderBuilder;
    use alloy_transport::mock::Asserter;
    use revm::database::InMemoryDB;

    use super::*;

    const AGGREGATOR: Address = address!("0x68bE4C50235205Ede361ac8244B1ee221CDDA5E2");
    const PROXY: Address = address!("0x7e860098F58bBFC8648a4311b374B1D669a2bc6B");

    /// Base mainnet USDC/USD (OCR2) storage at block 51951686.
    const HOT_VARS: U256 = uint!(0x2900004b9a0603_U256);
    const TRANSMISSION: U256 =
        uint!(0x6abbb3196abbb30b000000000000000000000000000000000000000005f5d5da_U256);

    fn mainnet_db() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        let layout = ChainlinkLayout::Ocr2;
        db.insert_account_storage(AGGREGATOR, layout.hot_vars_slot(), HOT_VARS).unwrap();
        db.insert_account_storage(AGGREGATOR, layout.transmission_slot(41), TRANSMISSION).unwrap();
        db
    }

    fn feed() -> ChainlinkFeed {
        ChainlinkFeed { aggregator: AGGREGATOR, layout: ChainlinkLayout::Ocr2, decimals: 8 }
    }

    fn latest_round(answer: i64) -> Bytes {
        Bytes::from(IChainlinkAggregator::latestRoundDataCall::abi_encode_returns(
            &IChainlinkAggregator::latestRoundDataReturn {
                roundId: U80::from(41),
                answer: I256::try_from(answer).unwrap(),
                startedAt: U256::from(1_790_685_963u64),
                updatedAt: U256::from(1_790_685_977u64),
                answeredInRound: U80::from(41),
            },
        ))
    }

    /// Queues the proxy and `typeAndVersion()` responses [`ChainlinkFeed::resolve`]
    /// requests first, in order.
    fn proxy_responses(type_and_version: &str) -> Asserter {
        let asserter = Asserter::new();
        asserter.push_success(&U64::from(51_951_686u64));
        asserter.push_success(&Bytes::from(IChainlinkProxy::aggregatorCall::abi_encode_returns(
            &AGGREGATOR,
        )));
        asserter.push_success(&Bytes::from(IChainlinkProxy::decimalsCall::abi_encode_returns(&8)));
        asserter.push_success(&Bytes::from(
            IChainlinkAggregator::typeAndVersionCall::abi_encode_returns(
                &type_and_version.to_owned(),
            ),
        ));
        asserter
    }

    /// Queues every response [`ChainlinkFeed::resolve`] requests, in order.
    fn resolve_responses(latest: Bytes) -> Asserter {
        let asserter = proxy_responses(ChainlinkLayout::OCR2_TYPE_AND_VERSION);
        asserter.push_success(&HOT_VARS);
        asserter.push_success(&TRANSMISSION);
        asserter.push_success(&latest);
        asserter
    }

    #[test]
    fn reads_mainnet_answer_from_storage() {
        let answer = feed().read(&mut mainnet_db()).unwrap();

        assert_eq!(
            answer,
            FeedAnswer {
                round_id: 41,
                answer: U256::from(99_997_146u64),
                updated_at: 1_790_685_977
            }
        );
    }

    #[test]
    fn rejects_feed_without_rounds() {
        let error = feed().read(&mut InMemoryDB::default()).unwrap_err();

        assert!(matches!(
            error,
            FeedReadError::InvalidAnswer { reason: InvalidAnswer::NoRounds, .. }
        ));
    }

    #[test]
    fn rejects_non_positive_answers() {
        let negative = (U256::from(1) << 191) | U256::from(5);

        assert_eq!(
            FeedAnswer::decode(3, negative),
            Err(InvalidAnswer::NonPositive { round_id: 3 })
        );
        assert_eq!(
            FeedAnswer::decode(3, U256::from(1_790_685_977u64) << 224),
            Err(InvalidAnswer::NonPositive { round_id: 3 })
        );
    }

    #[tokio::test]
    async fn resolves_proxy_when_storage_matches_latest_round() {
        let provider = ProviderBuilder::new()
            .connect_mocked_client(resolve_responses(latest_round(99_997_146)));

        assert_eq!(ChainlinkFeed::resolve(&provider, PROXY).await.unwrap(), feed());
    }

    #[tokio::test]
    async fn rejects_layout_that_disagrees_with_latest_round() {
        let provider = ProviderBuilder::new()
            .connect_mocked_client(resolve_responses(latest_round(100_000_000)));

        let error = ChainlinkFeed::resolve(&provider, PROXY).await.unwrap_err();

        assert!(matches!(
            error,
            FeedResolveError::LayoutMismatch {
                aggregator: AGGREGATOR,
                layout: ChainlinkLayout::Ocr2
            }
        ));
    }

    #[tokio::test]
    async fn rejects_unknown_aggregator_implementation() {
        let provider =
            ProviderBuilder::new().connect_mocked_client(proxy_responses("OCR2Aggregator 2.0.0"));

        let error = ChainlinkFeed::resolve(&provider, PROXY).await.unwrap_err();

        assert!(matches!(error, FeedResolveError::UnsupportedAggregator { .. }));
    }
}
