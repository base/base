//! L2 block timestamp scheduling.

use core::num::NonZeroU64;

/// The deterministic L2 block timestamp schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockTimestampSchedule {
    /// Absolute L2 genesis block number.
    pub genesis_block_number: u64,
    /// L2 genesis timestamp in seconds.
    pub genesis_timestamp: u64,
    /// Legacy block interval in seconds.
    pub legacy_block_interval: NonZeroU64,
    /// Denim activation timestamp.
    pub denim_activation_timestamp: u64,
}

impl BlockTimestampSchedule {
    /// Milliseconds between blocks after Denim.
    pub const DENIM_BLOCK_INTERVAL_MILLIS: u64 = 200;

    /// Returns the L2 block offset from genesis at which Denim activates.
    pub const fn denim_activation_block_number(&self) -> u64 {
        self.denim_activation_timestamp
            .saturating_sub(self.genesis_timestamp)
            .div_ceil(self.legacy_block_interval.get())
    }

    /// Returns whether the absolute block number is in the Denim schedule.
    pub const fn is_denim_active_at_block(&self, block_number: u64) -> bool {
        block_number.saturating_sub(self.genesis_block_number)
            >= self.denim_activation_block_number()
    }

    /// Returns the deterministic timestamp of an L2 block in milliseconds.
    pub const fn block_timestamp_millis(&self, block_number: u64) -> u64 {
        let blocks_since_genesis = block_number.saturating_sub(self.genesis_block_number);
        let legacy_seconds = self
            .genesis_timestamp
            .saturating_add(blocks_since_genesis.saturating_mul(self.legacy_block_interval.get()));
        let legacy_millis = legacy_seconds.saturating_mul(1_000);
        let denim_activation_block = self.denim_activation_block_number();

        if blocks_since_genesis < denim_activation_block {
            return legacy_millis;
        }

        let denim_activation_millis = self
            .genesis_timestamp
            .saturating_add(denim_activation_block.saturating_mul(self.legacy_block_interval.get()))
            .saturating_mul(1_000);
        denim_activation_millis.saturating_add(
            blocks_since_genesis
                .saturating_sub(denim_activation_block)
                .saturating_mul(Self::DENIM_BLOCK_INTERVAL_MILLIS),
        )
    }

    /// Returns the deterministic timestamp split into `(seconds, millis_part)`.
    pub const fn block_timestamp_parts(&self, block_number: u64) -> (u64, u16) {
        let millis = self.block_timestamp_millis(block_number);
        (millis.saturating_div(1_000), (millis % 1_000) as u16)
    }
}
