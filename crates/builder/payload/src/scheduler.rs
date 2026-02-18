/// Flashblock timing scheduler.
use core::time::Duration;
use std::ops::{Div, Rem};

use tracing::error;

use crate::PayloadBuilderConfig;

/// Flashblock timing scheduler.
///
/// Computes the number of flashblocks to produce and the timing offset
/// for the first flashblock based on configuration and FCU arrival time.
#[derive(Debug)]
pub struct FlashblockScheduler;

impl FlashblockScheduler {
    /// Calculate the number of flashblocks and the first flashblock timing offset.
    ///
    /// If `fixed` mode is enabled, returns the configured flashblocks-per-block count
    /// and uses `interval - leeway_time` as the first offset.
    ///
    /// If dynamic mode is enabled, adjusts the flashblock count based on how much
    /// time remains until the block deadline (accounting for FCU arrival drift).
    pub fn schedule(config: &PayloadBuilderConfig, timestamp: u64) -> (u64, Duration) {
        if config.flashblocks.fixed {
            return (
                config.flashblocks_per_block(),
                // We adjust first FB to ensure that we have at least some time to make all FB in time
                config.flashblocks.interval - config.flashblocks.leeway_time,
            );
        }

        // We use this system time to determine remaining time to build a block
        // Things to consider:
        // FCU(a) - FCU with attributes
        // FCU(a) could arrive with `block_time - fb_time < delay`. In this case we could only produce 1 flashblock
        // FCU(a) could arrive with `delay < fb_time` - in this case we will shrink first flashblock
        // FCU(a) could arrive with `fb_time < delay < block_time - fb_time` - in this case we will issue less flashblocks
        let target_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)
            - config.flashblocks.leeway_time;
        let now = std::time::SystemTime::now();
        let Some(time_drift) =
            target_time.duration_since(now).ok().filter(|duration| duration.as_millis() > 0)
        else {
            error!(
                target: "payload_builder",
                message = "FCU arrived too late or system clock are unsynced",
                ?target_time,
                ?now,
            );
            return (config.flashblocks_per_block(), config.flashblocks.interval);
        };
        // This is extra check to ensure that we would account at least for block time in case we have any timer discrepancies.
        let time_drift = time_drift.min(config.block_time);
        let interval = config.flashblocks.interval.as_millis() as u64;
        let time_drift = time_drift.as_millis() as u64;
        let first_flashblock_offset = time_drift.rem(interval);
        if first_flashblock_offset == 0 {
            // We have perfect division, so we use interval as first fb offset
            (time_drift.div(interval), Duration::from_millis(interval))
        } else {
            // Non-perfect division, so we account for it.
            (time_drift.div(interval) + 1, Duration::from_millis(first_flashblock_offset))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlashblocksConfig;

    fn test_config() -> PayloadBuilderConfig {
        PayloadBuilderConfig {
            block_time: Duration::from_secs(2),
            flashblocks: FlashblocksConfig {
                interval: Duration::from_millis(250),
                leeway_time: Duration::from_millis(50),
                fixed: false,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn fixed_mode_returns_configured_count_and_adjusted_interval() {
        let mut config = test_config();
        config.flashblocks.fixed = true;
        // block_time=2000ms / interval=250ms = 8 flashblocks
        let (count, offset) = FlashblockScheduler::schedule(&config, 0);
        assert_eq!(count, 8);
        // offset = interval - leeway = 250 - 50 = 200ms
        assert_eq!(offset, Duration::from_millis(200));
    }

    #[test]
    fn fixed_mode_with_different_leeway() {
        let mut config = test_config();
        config.flashblocks.fixed = true;
        config.flashblocks.leeway_time = Duration::from_millis(100);
        let (count, offset) = FlashblockScheduler::schedule(&config, 0);
        assert_eq!(count, 8);
        // offset = 250 - 100 = 150ms
        assert_eq!(offset, Duration::from_millis(150));
    }

    #[test]
    fn dynamic_mode_with_plenty_of_time() {
        let config = test_config();
        // Set timestamp far in the future so there's plenty of time
        let now =
            std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap();
        let future_timestamp = now.as_secs() + 2; // 2 seconds from now
        let (count, offset) = FlashblockScheduler::schedule(&config, future_timestamp);
        // With ~2s remaining and 250ms interval, we should get ~8 flashblocks
        // The exact count depends on timing but should be > 0
        assert!(count > 0, "Should produce at least one flashblock");
        assert!(offset <= Duration::from_millis(250), "First offset should not exceed interval");
    }

    #[test]
    fn dynamic_mode_late_fcu_falls_back() {
        let config = test_config();
        // Use a timestamp in the past, simulating a late FCU
        let now =
            std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap();
        let past_timestamp = now.as_secs().saturating_sub(10);
        let (count, offset) = FlashblockScheduler::schedule(&config, past_timestamp);
        // When FCU is too late, falls back to flashblocks_per_block and interval
        assert_eq!(count, config.flashblocks_per_block());
        assert_eq!(offset, config.flashblocks.interval);
    }

    #[test]
    fn zero_block_time_returns_zero_flashblocks_in_fixed_mode() {
        let mut config = test_config();
        config.block_time = Duration::ZERO;
        config.flashblocks.fixed = true;
        let (count, _) = FlashblockScheduler::schedule(&config, 0);
        // flashblocks_per_block returns 0 when block_time is 0
        assert_eq!(count, 0);
    }

    #[test]
    fn dynamic_mode_time_drift_capped_at_block_time() {
        let mut config = test_config();
        config.block_time = Duration::from_secs(1);
        config.flashblocks.interval = Duration::from_millis(200);
        config.flashblocks.leeway_time = Duration::from_millis(0);
        // Set timestamp far in the future - drift should be capped at block_time (1s)
        let now =
            std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap();
        let future_timestamp = now.as_secs() + 100; // way in the future
        let (count, _) = FlashblockScheduler::schedule(&config, future_timestamp);
        // With 1s block_time and 200ms interval, max 5 flashblocks
        assert!(count <= 5, "Count should be capped by block_time: got {count}");
    }
}
