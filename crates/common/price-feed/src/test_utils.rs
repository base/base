//! Contract fixtures for tests that read feeds and tokens from local state.
//!
//! [`ViewContract`] assembles runtime bytecode directly, so fixtures need no
//! compiled Solidity artifacts.

use alloy_primitives::{Address, Bytes, I256, U256, aliases::U80};
use alloy_sol_types::SolCall;

use crate::{ChainlinkFeed, ChainlinkLayout, FeedAnswer, IChainlinkAggregator, IChainlinkProxy};

/// Runtime bytecode answering fixed selectors with fixed data.
///
/// Arguments are ignored, and any other selector reverts with no data.
#[derive(Debug, Clone, Default)]
pub struct ViewContract {
    entries: Vec<ViewEntry>,
}

/// One selector a [`ViewContract`] answers.
#[derive(Debug, Clone)]
pub struct ViewEntry {
    /// Function selector.
    pub selector: [u8; 4],
    /// Whether the call reverts with [`Self::data`] rather than returning it.
    pub reverts: bool,
    /// Return or revert data.
    pub data: Bytes,
}

impl ViewContract {
    /// Bytes loading the selector: `PUSH1 0 CALLDATALOAD PUSH1 0xe0 SHR`.
    const HEADER_LEN: usize = 6;
    /// Bytes per dispatch: `DUP1 PUSH4 selector EQ PUSH2 target JUMPI`.
    const DISPATCH_LEN: usize = 11;
    /// Bytes of the unmatched-selector fallback: `PUSH1 0 DUP1 REVERT`.
    const FALLBACK_LEN: usize = 4;
    /// Bytes per body: `JUMPDEST PUSH2 len PUSH2 offset PUSH1 0 CODECOPY PUSH2 len PUSH1 0 RETURN`.
    const BODY_LEN: usize = 16;

    /// Creates a contract that answers no selector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `data` for calls to `selector`.
    pub fn returns(mut self, selector: [u8; 4], data: impl Into<Bytes>) -> Self {
        self.entries.push(ViewEntry { selector, reverts: false, data: data.into() });
        self
    }

    /// Reverts with `data` for calls to `selector`.
    pub fn reverts(mut self, selector: [u8; 4], data: impl Into<Bytes>) -> Self {
        self.entries.push(ViewEntry { selector, reverts: true, data: data.into() });
        self
    }

    /// Assembles the runtime bytecode.
    pub fn bytecode(&self) -> Bytes {
        let bodies_start =
            Self::HEADER_LEN + self.entries.len() * Self::DISPATCH_LEN + Self::FALLBACK_LEN;
        let data_start = bodies_start + self.entries.len() * Self::BODY_LEN;

        let mut code = vec![0x60, 0x00, 0x35, 0x60, 0xe0, 0x1c];
        for (index, entry) in self.entries.iter().enumerate() {
            code.extend([0x80, 0x63]);
            code.extend(entry.selector);
            code.extend([0x14, 0x61]);
            code.extend(Self::u16(bodies_start + index * Self::BODY_LEN));
            code.push(0x57);
        }
        code.extend([0x60, 0x00, 0x80, 0xfd]);

        let mut offset = data_start;
        for entry in &self.entries {
            let len = Self::u16(entry.data.len());
            code.extend([0x5b, 0x61]);
            code.extend(len);
            code.push(0x61);
            code.extend(Self::u16(offset));
            code.extend([0x60, 0x00, 0x39, 0x61]);
            code.extend(len);
            code.extend([0x60, 0x00, if entry.reverts { 0xfd } else { 0xf3 }]);
            offset += entry.data.len();
        }
        for entry in &self.entries {
            code.extend_from_slice(&entry.data);
        }
        code.into()
    }

    fn u16(value: usize) -> [u8; 2] {
        u16::try_from(value).expect("view contract fits in 64 KiB").to_be_bytes()
    }
}

/// Creation bytecode that writes fixed storage and deploys fixed runtime,
/// for installing fixtures on a live chain.
#[derive(Debug, Clone, Copy)]
pub struct InitCode;

impl InitCode {
    /// Bytes per write: `PUSH32 value PUSH32 slot SSTORE`.
    const WRITE_LEN: usize = 67;
    /// Bytes of `PUSH2 len DUP1 PUSH2 offset PUSH1 0 CODECOPY PUSH1 0 RETURN`.
    const RETURN_LEN: usize = 13;

    /// Assembles creation code storing each `(slot, value)` then returning
    /// `runtime`.
    pub fn deploying(runtime: &[u8], storage: &[(U256, U256)]) -> Bytes {
        let mut code =
            Vec::with_capacity(storage.len() * Self::WRITE_LEN + Self::RETURN_LEN + runtime.len());
        for (slot, value) in storage {
            code.push(0x7f);
            code.extend(value.to_be_bytes::<32>());
            code.push(0x7f);
            code.extend(slot.to_be_bytes::<32>());
            code.push(0x55);
        }
        let len = ViewContract::u16(runtime.len());
        code.push(0x61);
        code.extend(len);
        code.extend([0x80, 0x61]);
        code.extend(ViewContract::u16(storage.len() * Self::WRITE_LEN + Self::RETURN_LEN));
        code.extend([0x60, 0x00, 0x39, 0x60, 0x00, 0xf3]);
        code.extend_from_slice(runtime);
        code.into()
    }
}

/// A single-round OCR2 Chainlink feed behind its proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockFeed {
    /// Feed proxy.
    pub proxy: Address,
    /// Aggregator the proxy delegates to.
    pub aggregator: Address,
    /// Decimals of the answer.
    pub decimals: u8,
    /// Latest round.
    pub round_id: u32,
    /// Latest answer.
    pub answer: U256,
    /// Unix timestamp, in seconds, of the latest round.
    pub updated_at: u64,
}

impl MockFeed {
    /// Creates a feed at 8 decimals whose proxy and aggregator addresses are
    /// derived from `byte`.
    pub fn new(byte: u8, answer: u64) -> Self {
        Self {
            proxy: Address::repeat_byte(byte),
            aggregator: Address::repeat_byte(byte.wrapping_add(0x10)),
            decimals: 8,
            round_id: 1,
            answer: U256::from(answer),
            updated_at: 1_790_685_977,
        }
    }

    /// Feed [`ChainlinkFeed::resolve`] resolves the proxy to.
    pub const fn feed(&self) -> ChainlinkFeed {
        ChainlinkFeed {
            aggregator: self.aggregator,
            layout: ChainlinkLayout::Ocr2,
            decimals: self.decimals,
        }
    }

    /// Runtime of the proxy.
    pub fn proxy_code(&self) -> Bytes {
        ViewContract::new()
            .returns(
                IChainlinkProxy::aggregatorCall::SELECTOR,
                IChainlinkProxy::aggregatorCall::abi_encode_returns(&self.aggregator),
            )
            .returns(
                IChainlinkProxy::decimalsCall::SELECTOR,
                IChainlinkProxy::decimalsCall::abi_encode_returns(&self.decimals),
            )
            .bytecode()
    }

    /// Runtime of the aggregator, reporting `answer` from `latestRoundData()`.
    ///
    /// Passing an answer other than [`Self::answer`] makes the storage layout
    /// disagree with the aggregator.
    pub fn aggregator_code(&self, answer: U256) -> Bytes {
        let round = IChainlinkAggregator::latestRoundDataReturn {
            roundId: U80::from(self.round_id),
            answer: I256::from_raw(answer),
            startedAt: U256::from(self.updated_at),
            updatedAt: U256::from(self.updated_at),
            answeredInRound: U80::from(self.round_id),
        };
        ViewContract::new()
            .returns(
                IChainlinkAggregator::typeAndVersionCall::SELECTOR,
                IChainlinkAggregator::typeAndVersionCall::abi_encode_returns(
                    &ChainlinkLayout::OCR2_TYPE_AND_VERSION.to_owned(),
                ),
            )
            .returns(
                IChainlinkAggregator::latestRoundDataCall::SELECTOR,
                IChainlinkAggregator::latestRoundDataCall::abi_encode_returns(&round),
            )
            .bytecode()
    }

    /// Aggregator storage holding the latest round.
    pub fn aggregator_storage(&self) -> [(U256, U256); 2] {
        let layout = ChainlinkLayout::Ocr2;
        let hot_vars = U256::from(self.round_id) << ChainlinkLayout::ROUND_ID_BIT_OFFSET;
        let transmission =
            self.answer | (U256::from(self.updated_at) << FeedAnswer::UPDATED_AT_BIT_OFFSET);
        [
            (layout.hot_vars_slot(), hot_vars),
            (layout.transmission_slot(self.round_id), transmission),
        ]
    }
}
