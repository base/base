//! Chainlink feed reads from aggregator storage.

use alloy_primitives::{Address, I256, U256, aliases::U80};
use alloy_sol_types::sol;
use revm::Database;

use crate::{ChainlinkLayout, StateCall, StateCallError};

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
pub enum FeedResolveError<E> {
    /// A proxy or aggregator call failed.
    #[error(transparent)]
    Call(#[from] StateCallError<E>),
    /// The aggregator's latest round could not be read from storage.
    #[error(transparent)]
    Read(#[from] FeedReadError<E>),
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

    /// Resolves `proxy` to its current aggregator in `db` and verifies the
    /// aggregator's storage layout.
    ///
    /// The feed is accepted only if the storage read equals `latestRoundData()`
    /// in the same state.
    pub fn resolve<DB: Database>(
        db: &mut DB,
        proxy: Address,
    ) -> Result<Self, FeedResolveError<DB::Error>> {
        let aggregator = StateCall::call(db, proxy, &IChainlinkProxy::aggregatorCall {})?;
        let decimals = StateCall::call(db, proxy, &IChainlinkProxy::decimalsCall {})?;
        let type_and_version =
            StateCall::call(db, aggregator, &IChainlinkAggregator::typeAndVersionCall {})?;
        let Some(layout) = ChainlinkLayout::from_type_and_version(&type_and_version) else {
            return Err(FeedResolveError::UnsupportedAggregator {
                proxy,
                aggregator,
                type_and_version,
            });
        };
        let feed = Self { aggregator, layout, decimals };
        let answer = feed.read(db)?;
        let latest =
            StateCall::call(db, aggregator, &IChainlinkAggregator::latestRoundDataCall {})?;
        if !answer.matches(&latest) {
            return Err(FeedResolveError::LayoutMismatch { aggregator, layout });
        }
        Ok(feed)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address, uint};
    use alloy_sol_types::SolCall;
    use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

    use super::*;
    use crate::test_utils::{MockFeed, ViewContract};

    const AGGREGATOR: Address = address!("0x68bE4C50235205Ede361ac8244B1ee221CDDA5E2");

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

    fn install(db: &mut InMemoryDB, address: Address, code: Bytes) {
        db.insert_account_info(address, AccountInfo::default().with_code(Bytecode::new_raw(code)));
    }

    /// State holding `mock` behind its proxy, with the aggregator reporting
    /// `reported` from `latestRoundData()`.
    fn feed_db(mock: &MockFeed, reported: U256) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        install(&mut db, mock.proxy, mock.proxy_code());
        install(&mut db, mock.aggregator, mock.aggregator_code(reported));
        for (slot, value) in mock.aggregator_storage() {
            db.insert_account_storage(mock.aggregator, slot, value).unwrap();
        }
        db
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

    #[test]
    fn resolves_proxy_when_storage_matches_latest_round() {
        let mock = MockFeed::new(0xa0, 99_997_146);
        let mut db = feed_db(&mock, mock.answer);

        assert_eq!(ChainlinkFeed::resolve(&mut db, mock.proxy).unwrap(), mock.feed());
    }

    #[test]
    fn rejects_layout_that_disagrees_with_latest_round() {
        let mock = MockFeed::new(0xa0, 99_997_146);
        let mut db = feed_db(&mock, U256::from(100_000_000));

        let error = ChainlinkFeed::resolve(&mut db, mock.proxy).unwrap_err();

        assert!(matches!(
            error,
            FeedResolveError::LayoutMismatch { aggregator, layout: ChainlinkLayout::Ocr2 }
                if aggregator == mock.aggregator
        ));
    }

    #[test]
    fn rejects_unknown_aggregator_implementation() {
        let mock = MockFeed::new(0xa0, 99_997_146);
        let mut db = feed_db(&mock, mock.answer);
        install(
            &mut db,
            mock.aggregator,
            ViewContract::new()
                .returns(
                    IChainlinkAggregator::typeAndVersionCall::SELECTOR,
                    IChainlinkAggregator::typeAndVersionCall::abi_encode_returns(
                        &"OCR2Aggregator 2.0.0".to_owned(),
                    ),
                )
                .bytecode(),
        );

        let error = ChainlinkFeed::resolve(&mut db, mock.proxy).unwrap_err();

        assert!(matches!(error, FeedResolveError::UnsupportedAggregator { .. }));
    }

    #[test]
    fn rejects_proxy_without_code() {
        let error = ChainlinkFeed::resolve(&mut InMemoryDB::default(), AGGREGATOR).unwrap_err();

        assert!(matches!(error, FeedResolveError::Call(StateCallError::Decode { .. })));
    }
}
