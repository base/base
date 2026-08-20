//! Encoder configuration and its validation error type.

use base_common_genesis::RollupConfig;
use base_protocol::{
    BLOB_DERIVATION_PREFIX_SIZE as PROTOCOL_BLOB_DERIVATION_PREFIX_SIZE,
    BLOB_MAX_DATA_SIZE as PROTOCOL_BLOB_MAX_DATA_SIZE, Frame,
    MAX_BLOB_FRAME_SIZE as PROTOCOL_MAX_BLOB_FRAME_SIZE,
};

use crate::{CompressionAlgo, DaType};

/// Configuration for the [`BatchEncoder`](crate::BatchEncoder).
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Optional soft compressed-byte target. The reaching batch stays, then the
    /// channel closes. `None` closes on duration, flush, or protocol limits.
    /// Default: `None`.
    pub compressed_size_target: Option<usize>,

    /// Maximum serialized derivation frame size.
    pub max_frame_size: usize,

    /// Maximum L1 blocks a channel may stay open.
    /// Default: 2.
    pub max_channel_duration: u64,

    /// Subtracted from `max_channel_duration` for the close deadline.
    /// Default: 0.
    pub sub_safety_margin: u64,

    /// Max blobs per L1 transaction. Does not close channels.
    /// Default: 6.
    pub max_blobs_per_tx: usize,

    /// How frames are encoded for L1 submission.
    /// Default: [`DaType::Blob`].
    pub da_type: DaType,

    /// Compression for newly opened channels. Brotli requires Fjord.
    /// Default: [`CompressionAlgo::Brotli`] at
    /// [`CompressionAlgo::BROTLI_DEFAULT_QUALITY`].
    pub compression_algo: CompressionAlgo,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            compressed_size_target: None,
            max_frame_size: Self::MAX_BLOB_FRAME_SIZE,
            max_channel_duration: 2,
            sub_safety_margin: 0,
            max_blobs_per_tx: 6,
            da_type: DaType::Blob,
            compression_algo: CompressionAlgo::Brotli(CompressionAlgo::BROTLI_DEFAULT_QUALITY),
        }
    }
}

impl EncoderConfig {
    /// Maximum number of bytes that can be encoded into one blob payload.
    pub const BLOB_MAX_DATA_SIZE: usize = PROTOCOL_BLOB_MAX_DATA_SIZE;

    /// Size of the derivation-version prefix prepended to each blob payload.
    pub const BLOB_DERIVATION_PREFIX_SIZE: usize = PROTOCOL_BLOB_DERIVATION_PREFIX_SIZE;

    /// Largest serialized frame that can fit in one blob after reserving the
    /// derivation-version prefix.
    pub const MAX_BLOB_FRAME_SIZE: usize = PROTOCOL_MAX_BLOB_FRAME_SIZE;

    /// Fusaka hard limit for blobs carried by one transaction.
    pub const MAX_BLOBS_PER_TX: usize = 6;

    /// Validate the configuration, returning an error if any constraint is violated.
    ///
    /// [`BatchEncoder::new`](crate::BatchEncoder::new) calls this automatically.
    /// It remains public for CLI and service startup validation.
    pub fn validate(&self) -> Result<(), EncoderConfigError> {
        // Channel timing must leave at least one L1 block of usable duration.
        if self.sub_safety_margin >= self.max_channel_duration {
            return Err(EncoderConfigError::SafetyMarginTooLarge {
                sub_safety_margin: self.sub_safety_margin,
                max_channel_duration: self.max_channel_duration,
            });
        }

        // Frame limits must satisfy derivation framing before DA-specific checks.
        // Every frame must carry at least one complete channel byte.
        let min_frame_size = Frame::ENCODED_OVERHEAD + 1;
        if self.max_frame_size < min_frame_size {
            return Err(EncoderConfigError::FrameSizeTooSmall {
                max_frame_size: self.max_frame_size,
                min_frame_size,
            });
        }

        let max_protocol_frame_size = Frame::ENCODED_OVERHEAD + Frame::MAX_LEN;
        if self.max_frame_size > max_protocol_frame_size {
            return Err(EncoderConfigError::FrameSizeTooLarge {
                max_frame_size: self.max_frame_size,
                max_protocol_frame_size,
            });
        }

        // Channel and transaction targets are independently configurable, but
        // neither accepts a zero-sized unit of work.
        if self.compressed_size_target == Some(0) {
            return Err(EncoderConfigError::CompressedTargetZero);
        }
        if self.max_blobs_per_tx == 0 {
            return Err(EncoderConfigError::MaxBlobsPerTxZero);
        }
        if self.max_blobs_per_tx > Self::MAX_BLOBS_PER_TX {
            return Err(EncoderConfigError::TooManyBlobsPerTx {
                configured: self.max_blobs_per_tx,
                maximum: Self::MAX_BLOBS_PER_TX,
            });
        }

        // All frames remain packable as blobs, including calldata configurations
        // that may switch to blob DA while throttling.
        if self.max_frame_size > Self::MAX_BLOB_FRAME_SIZE {
            return Err(EncoderConfigError::FrameExceedsBlobPackingLimit {
                max_frame_size: self.max_frame_size,
                max_blob_frame_size: Self::MAX_BLOB_FRAME_SIZE,
            });
        }

        if let CompressionAlgo::Brotli(quality) = self.compression_algo
            && quality > CompressionAlgo::BROTLI_MAX_QUALITY
        {
            return Err(EncoderConfigError::BrotliQualityOutOfRange { quality });
        }

        Ok(())
    }

    /// Minimum of pre- and post-Granite `channel_timeout`; `0` is treated as unset.
    pub fn confirmation_channel_timeout(rollup_config: &RollupConfig) -> u64 {
        let pre_granite = rollup_config.channel_timeout(0);
        let post_granite = rollup_config.channel_timeout(u64::MAX);
        match (pre_granite, post_granite) {
            (0, timeout) | (timeout, 0) => timeout,
            (pre, post) => pre.min(post),
        }
    }

    /// Validate the configuration against the active rollup state.
    ///
    /// `next_l2_timestamp` should be the timestamp of the next L2 block the
    /// batcher may encode. Brotli compression is only valid once Fjord is active
    /// for that next block.
    pub fn validate_for_rollup_config(
        &self,
        rollup_config: &RollupConfig,
        next_l2_timestamp: u64,
    ) -> Result<(), EncoderConfigError> {
        self.validate()?;

        if !matches!(self.compression_algo, CompressionAlgo::Zlib)
            && !rollup_config.is_fjord_active(next_l2_timestamp)
        {
            return Err(EncoderConfigError::BrotliRequiresFjord { next_l2_timestamp });
        }

        let channel_timeout = Self::confirmation_channel_timeout(rollup_config);
        if channel_timeout > 0 && self.max_channel_duration >= channel_timeout {
            return Err(EncoderConfigError::ChannelDurationExceedsTimeout {
                max_channel_duration: self.max_channel_duration,
                channel_timeout,
            });
        }

        Ok(())
    }
}

/// Errors returned when validating [`EncoderConfig`].
#[derive(Debug, thiserror::Error)]
pub enum EncoderConfigError {
    /// `sub_safety_margin >= max_channel_duration`.
    ///
    /// The effective channel timeout (`max_channel_duration - sub_safety_margin`) would
    /// saturate to 0, causing every channel to close immediately on the first
    /// `advance_l1_head` call. Ensure `sub_safety_margin < max_channel_duration`.
    #[error(
        "sub_safety_margin ({sub_safety_margin}) must be less than \
         max_channel_duration ({max_channel_duration})"
    )]
    SafetyMarginTooLarge {
        /// The configured safety margin.
        sub_safety_margin: u64,
        /// The configured maximum channel duration.
        max_channel_duration: u64,
    },
    /// `max_frame_size` cannot carry one complete channel byte.
    #[error(
        "max_frame_size ({max_frame_size}) must be at least {min_frame_size} bytes \
         to carry frame metadata and payload"
    )]
    FrameSizeTooSmall {
        /// The configured maximum frame size.
        max_frame_size: usize,
        /// The minimum frame size that can carry channel data.
        min_frame_size: usize,
    },
    /// `max_frame_size` exceeds the protocol decoder limit.
    #[error("max_frame_size ({max_frame_size}) must be at most {max_protocol_frame_size} bytes")]
    FrameSizeTooLarge {
        /// Configured maximum serialized frame size.
        max_frame_size: usize,
        /// Largest serialized frame accepted by the protocol decoder.
        max_protocol_frame_size: usize,
    },
    /// `compressed_size_target == Some(0)`.
    #[error("compressed_size_target must be greater than zero when configured")]
    CompressedTargetZero,
    /// `max_blobs_per_tx == 0`.
    #[error("max_blobs_per_tx must be greater than zero")]
    MaxBlobsPerTxZero,
    /// `max_blobs_per_tx` exceeds the protocol transaction limit.
    #[error("max_blobs_per_tx ({configured}) must be at most {maximum}")]
    TooManyBlobsPerTx {
        /// Configured blob count.
        configured: usize,
        /// Protocol maximum blob count.
        maximum: usize,
    },
    /// `max_frame_size` leaves no room for a blob derivation-version prefix.
    ///
    /// This also applies to calldata configurations because DA throttling may
    /// switch any encoder to blob submissions at runtime.
    #[error(
        "max_frame_size ({max_frame_size}) must be at most \
         {max_blob_frame_size} to leave room for the derivation-version prefix"
    )]
    FrameExceedsBlobPackingLimit {
        /// The configured maximum frame size.
        max_frame_size: usize,
        /// The maximum frame size that leaves room for the derivation-version prefix.
        max_blob_frame_size: usize,
    },
    /// Brotli quality is outside the encoder's accepted range.
    #[error("brotli quality {quality} is outside 0..=11")]
    BrotliQualityOutOfRange {
        /// The configured Brotli quality.
        quality: u8,
    },
    /// Brotli compression is configured before Fjord activates.
    #[error(
        "brotli compression requires Fjord to be active for the next L2 block; \
         next_l2_timestamp is {next_l2_timestamp}"
    )]
    BrotliRequiresFjord {
        /// The timestamp of the next L2 block the batcher may encode.
        next_l2_timestamp: u64,
    },
    /// `max_channel_duration >= channel_timeout`.
    #[error(
        "max_channel_duration ({max_channel_duration}) must be less than \
         the derivation channel_timeout ({channel_timeout})"
    )]
    ChannelDurationExceedsTimeout {
        /// Configured duration in L1 blocks.
        max_channel_duration: u64,
        /// Derivation channel timeout in L1 blocks.
        channel_timeout: u64,
    },
}

#[cfg(test)]
mod tests {
    use base_common_genesis::UpgradeConfig;
    use rstest::rstest;

    use super::*;

    fn config_with(sub_safety_margin: u64, max_channel_duration: u64) -> EncoderConfig {
        EncoderConfig { sub_safety_margin, max_channel_duration, ..EncoderConfig::default() }
    }

    #[test]
    fn default_blob_max_frame_size_reserves_derivation_prefix() {
        let cfg = EncoderConfig::default();

        assert_eq!(cfg.max_frame_size, EncoderConfig::MAX_BLOB_FRAME_SIZE);
        assert_eq!(cfg.compressed_size_target, None);
        assert_eq!(cfg.max_blobs_per_tx, 6);
        assert_eq!(
            cfg.compression_algo,
            CompressionAlgo::Brotli(CompressionAlgo::BROTLI_DEFAULT_QUALITY)
        );
        assert_eq!(
            cfg.max_frame_size + EncoderConfig::BLOB_DERIVATION_PREFIX_SIZE,
            EncoderConfig::BLOB_MAX_DATA_SIZE
        );
    }

    #[rstest]
    #[case(0, 2)] // zero margin: always valid
    #[case(1, 2)] // one below duration
    #[case(4, 10)] // typical production values
    fn validate_ok(#[case] sub_safety_margin: u64, #[case] max_channel_duration: u64) {
        assert!(config_with(sub_safety_margin, max_channel_duration).validate().is_ok());
    }

    #[rstest]
    #[case(2, 2)] // equal: effective timeout saturates to 0
    #[case(5, 2)] // greater: same failure mode
    #[case(u64::MAX, 1)] // extreme: maximum possible margin
    fn validate_err(#[case] sub_safety_margin: u64, #[case] max_channel_duration: u64) {
        let err = config_with(sub_safety_margin, max_channel_duration).validate().unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::SafetyMarginTooLarge {
                sub_safety_margin: m,
                max_channel_duration: d,
            } if m == sub_safety_margin && d == max_channel_duration
        ));
        // Error message must be human-readable and include both values.
        let msg = err.to_string();
        assert!(msg.contains(&sub_safety_margin.to_string()));
        assert!(msg.contains(&max_channel_duration.to_string()));
    }

    #[rstest]
    #[case(CompressionAlgo::Zlib)]
    #[case(CompressionAlgo::Brotli(10))]
    fn validate_rejects_frame_without_payload_capacity(#[case] compression_algo: CompressionAlgo) {
        let max_frame_size = Frame::ENCODED_OVERHEAD;
        let min_frame_size = Frame::ENCODED_OVERHEAD + 1;
        let cfg = EncoderConfig { compression_algo, max_frame_size, ..EncoderConfig::default() };

        let err = cfg.validate().unwrap_err();

        assert!(matches!(
            err,
            EncoderConfigError::FrameSizeTooSmall {
                max_frame_size: actual,
                min_frame_size: minimum,
            } if actual == max_frame_size && minimum == min_frame_size
        ));
    }

    #[rstest]
    #[case(CompressionAlgo::Zlib)]
    #[case(CompressionAlgo::Brotli(10))]
    fn validate_accepts_frame_with_payload_capacity(#[case] compression_algo: CompressionAlgo) {
        let max_frame_size = Frame::ENCODED_OVERHEAD + 1;
        let cfg = EncoderConfig { compression_algo, max_frame_size, ..EncoderConfig::default() };

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_compressed_target() {
        let cfg = EncoderConfig { compressed_size_target: Some(0), ..EncoderConfig::default() };

        assert!(matches!(cfg.validate().unwrap_err(), EncoderConfigError::CompressedTargetZero));
    }

    #[test]
    fn validate_rejects_zero_max_blobs_per_tx() {
        let cfg = EncoderConfig { max_blobs_per_tx: 0, ..EncoderConfig::default() };

        assert!(matches!(cfg.validate().unwrap_err(), EncoderConfigError::MaxBlobsPerTxZero));
    }

    #[test]
    fn validate_rejects_transaction_above_blob_limit() {
        let cfg = EncoderConfig {
            max_blobs_per_tx: EncoderConfig::MAX_BLOBS_PER_TX + 1,
            ..EncoderConfig::default()
        };

        assert!(matches!(
            cfg.validate().unwrap_err(),
            EncoderConfigError::TooManyBlobsPerTx { configured: 7, maximum: 6 }
        ));
    }

    #[test]
    fn validate_rejects_frame_above_protocol_limit() {
        let cfg = EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: Frame::ENCODED_OVERHEAD + Frame::MAX_LEN + 1,
            ..EncoderConfig::default()
        };

        assert!(matches!(cfg.validate(), Err(EncoderConfigError::FrameSizeTooLarge { .. })));
    }

    #[test]
    fn validate_rejects_blob_frame_size_that_leaves_no_prefix_room() {
        let cfg = EncoderConfig {
            max_frame_size: EncoderConfig::BLOB_MAX_DATA_SIZE,
            ..EncoderConfig::default()
        };

        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::FrameExceedsBlobPackingLimit {
                max_frame_size,
                max_blob_frame_size,
            } if max_frame_size == EncoderConfig::BLOB_MAX_DATA_SIZE
                && max_blob_frame_size == EncoderConfig::MAX_BLOB_FRAME_SIZE
        ));
        assert!(err.to_string().contains("derivation-version prefix"));
    }

    #[test]
    fn validate_reserves_blob_prefix_room_for_calldata_override() {
        let cfg = EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: EncoderConfig::BLOB_MAX_DATA_SIZE,
            ..EncoderConfig::default()
        };

        assert!(matches!(
            cfg.validate(),
            Err(EncoderConfigError::FrameExceedsBlobPackingLimit { .. })
        ));
    }

    fn rollup_config_with(block_time: u64, fjord_time: Option<u64>) -> RollupConfig {
        RollupConfig {
            block_time,
            upgrades: UpgradeConfig { fjord_time, ..UpgradeConfig::default() },
            ..RollupConfig::default()
        }
    }

    #[test]
    fn validate_rejects_brotli_quality_above_max() {
        let cfg = EncoderConfig {
            compression_algo: CompressionAlgo::Brotli(12),
            ..EncoderConfig::default()
        };

        assert!(matches!(
            cfg.validate().unwrap_err(),
            EncoderConfigError::BrotliQualityOutOfRange { quality: 12 }
        ));
    }

    #[test]
    fn validate_accepts_brotli_quality_bounds() {
        for quality in [CompressionAlgo::BROTLI_MIN_QUALITY, CompressionAlgo::BROTLI_MAX_QUALITY] {
            let cfg = EncoderConfig {
                compression_algo: CompressionAlgo::Brotli(quality),
                ..EncoderConfig::default()
            };
            assert!(cfg.validate().is_ok());
        }
    }

    #[test]
    fn validate_for_rollup_config_allows_single_before_fjord() {
        let cfg =
            EncoderConfig { compression_algo: CompressionAlgo::Zlib, ..EncoderConfig::default() };
        let rollup_config = rollup_config_with(2, Some(100));

        assert!(cfg.validate_for_rollup_config(&rollup_config, 98).is_ok());
    }

    #[test]
    fn validate_for_rollup_config_rejects_brotli_before_fjord() {
        let cfg = EncoderConfig::default();
        let rollup_config = rollup_config_with(2, Some(100));

        let err = cfg.validate_for_rollup_config(&rollup_config, 98).unwrap_err();
        assert!(matches!(err, EncoderConfigError::BrotliRequiresFjord { next_l2_timestamp: 98 }));
    }

    fn rollup_config_with_channel_timeouts(
        pre_granite: u64,
        post_granite: u64,
        granite_time: Option<u64>,
    ) -> RollupConfig {
        RollupConfig {
            channel_timeout: pre_granite,
            granite_channel_timeout: post_granite,
            upgrades: UpgradeConfig { granite_time, ..UpgradeConfig::default() },
            ..RollupConfig::default()
        }
    }

    #[test]
    fn confirmation_channel_timeout_takes_the_conservative_minimum() {
        let rollup_config = rollup_config_with_channel_timeouts(300, 50, Some(10));
        assert_eq!(EncoderConfig::confirmation_channel_timeout(&rollup_config), 50);
    }

    #[test]
    fn confirmation_channel_timeout_treats_zero_as_unset() {
        let rollup_config = rollup_config_with_channel_timeouts(0, 50, Some(10));
        assert_eq!(EncoderConfig::confirmation_channel_timeout(&rollup_config), 50);
    }

    #[test]
    fn validate_for_rollup_config_rejects_duration_at_channel_timeout() {
        let cfg = EncoderConfig {
            compression_algo: CompressionAlgo::Zlib,
            max_channel_duration: 50,
            ..EncoderConfig::default()
        };
        let rollup_config = rollup_config_with_channel_timeouts(300, 50, Some(10));

        let err = cfg.validate_for_rollup_config(&rollup_config, 0).unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::ChannelDurationExceedsTimeout {
                max_channel_duration: 50,
                channel_timeout: 50,
            }
        ));
    }

    #[test]
    fn validate_for_rollup_config_allows_duration_below_channel_timeout() {
        let cfg = EncoderConfig {
            compression_algo: CompressionAlgo::Zlib,
            max_channel_duration: 49,
            ..EncoderConfig::default()
        };
        let rollup_config = rollup_config_with_channel_timeouts(300, 50, Some(10));

        assert!(cfg.validate_for_rollup_config(&rollup_config, 0).is_ok());
    }

    #[test]
    fn validate_for_rollup_config_skips_when_channel_timeout_is_unset() {
        let cfg = EncoderConfig {
            compression_algo: CompressionAlgo::Zlib,
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };

        assert!(cfg.validate_for_rollup_config(&RollupConfig::default(), 0).is_ok());
    }
}
