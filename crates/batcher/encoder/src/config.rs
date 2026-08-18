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
    /// Optional soft limit for a channel's compressed bytes.
    ///
    /// The batch that makes the compressor emit at least this many total bytes
    /// remains in the channel, then the channel closes. Bytes already assigned
    /// to full artifacts still count toward the target. Compressor buffering and
    /// the final complete batch may make the finished stream exceed it.
    /// This bounds channel latency and memory without a shadow compressor or
    /// rollback path. `None` closes channels only for protocol channel limits,
    /// duration, or an explicit flush.
    ///
    /// Default: `None`.
    pub compressed_size_target: Option<usize>,

    /// Maximum serialized size of each derivation frame.
    ///
    /// Streaming DA egress creates frames against the remaining artifact
    /// capacity without exceeding this bound. Set it smaller in tests to force
    /// additional frames.
    pub max_frame_size: usize,

    /// Maximum L1 blocks a channel may stay open.
    /// Default: 2.
    pub max_channel_duration: u64,

    /// Safety margin reserved within `max_channel_duration`.
    ///
    /// The encoder computes one operational deadline at channel open using
    /// `max_channel_duration - sub_safety_margin`. That deadline closes an open
    /// channel and later releases any partial blob retained after a size close.
    /// The default of 0 uses the full configured duration.
    /// Default: 0.
    pub sub_safety_margin: u64,

    /// Maximum number of blobs included in one L1 transaction.
    ///
    /// This controls transaction construction only; it does not close channels.
    /// Fusaka limits each transaction to six blobs; the separate block-level
    /// limit may be higher.
    ///
    /// Default: 6.
    pub max_blobs_per_tx: usize,

    /// How frames should be encoded for L1 submission.
    ///
    /// Default: [`DaType::Blob`].
    pub da_type: DaType,

    /// Compression algorithm used for newly opened channels.
    ///
    /// Brotli channels are accepted only after Fjord. Zlib remains valid on both
    /// sides of the fork and should be selected for pre-Fjord environments.
    ///
    /// Default: [`CompressionAlgo::Brotli10`].
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
            compression_algo: CompressionAlgo::Brotli10,
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

        Ok(())
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
    /// Brotli compression is configured before Fjord activates.
    #[error(
        "brotli compression requires Fjord to be active for the next L2 block; \
         next_l2_timestamp is {next_l2_timestamp}"
    )]
    BrotliRequiresFjord {
        /// The timestamp of the next L2 block the batcher may encode.
        next_l2_timestamp: u64,
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
    #[case(CompressionAlgo::Brotli10)]
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
    #[case(CompressionAlgo::Brotli10)]
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
}
