//! Encoder configuration and its validation error type.

use base_common_genesis::RollupConfig;
pub use base_comp::CompressionAlgo;
use base_protocol::{
    BLOB_DERIVATION_PREFIX_SIZE as PROTOCOL_BLOB_DERIVATION_PREFIX_SIZE,
    BLOB_MAX_DATA_SIZE as PROTOCOL_BLOB_MAX_DATA_SIZE, BatchType, Frame,
    MAX_BLOB_FRAME_SIZE as PROTOCOL_MAX_BLOB_FRAME_SIZE,
};

use crate::DaType;

/// Configuration for the [`BatchEncoder`](crate::BatchEncoder).
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Target serialized size of each frame used to derive the channel output limit.
    ///
    /// Frame metadata and the optional Brotli version byte are reserved before
    /// compression begins.
    /// Default: 130,043 bytes (`MAX_BLOB_FRAME_SIZE`).
    pub target_frame_size: usize,

    /// Maximum byte size of each output frame when draining a closed channel.
    ///
    /// Set smaller to force multi-frame output (e.g. in tests that exercise
    /// partial-channel submission and channel timeouts).
    pub max_frame_size: usize,

    /// Maximum L1 blocks a channel may stay open.
    /// Default: 2.
    pub max_channel_duration: u64,

    /// Safety margin subtracted from `max_channel_duration` when evaluating channel
    /// timeout. The effective timeout is `max_channel_duration - sub_safety_margin`,
    /// ensuring channels have closed this many L1 blocks before the configured
    /// duration expires.
    ///
    /// Set this large enough so that in-flight frames land well within the protocol's
    /// `channel_timeout` inclusion window. A margin of 4–10 is typical; the default
    /// of 0 means no margin (effective timeout equals `max_channel_duration` exactly).
    ///
    /// Note: if `sub_safety_margin >= max_channel_duration` the effective timeout
    /// saturates to 0 L1 blocks and every channel closes immediately on the next
    /// `advance_l1_head` call. Ensure `sub_safety_margin < max_channel_duration`.
    ///
    /// Default: 0.
    pub sub_safety_margin: u64,

    /// Target number of frames per channel and per L1 transaction.
    ///
    /// Each frame maps to one EIP-4844 blob, so setting this to N submits N blobs
    /// per transaction. Cancun supports up to 6; Isthmus (EIP-7892) up to 21.
    ///
    /// Default: 1 (one blob per transaction).
    pub target_num_frames: usize,

    /// Maximum number of L2 blocks to accumulate into one span batch.
    ///
    /// Reaching the limit seals the current span batch and starts another in the
    /// same channel. It does not close the channel. When unset, a channel contains
    /// one span batch.
    ///
    /// Default: `None`.
    pub max_blocks_per_span_batch: Option<usize>,

    /// Whether to encode blocks as individual
    /// [`SingleBatch`](base_protocol::batch::SingleBatch)es
    /// or group them into [`SpanBatch`](base_protocol::SpanBatch)es.
    ///
    /// Default: [`BatchType::Single`].
    pub batch_type: BatchType,

    /// How frames should be encoded for L1 submission.
    ///
    /// When set to [`DaType::Calldata`], set [`target_num_frames`] to `1` so
    /// that each [`BatchSubmission`](crate::BatchSubmission) contains exactly one frame
    /// (one calldata tx per frame matches the derivation protocol).
    ///
    /// Default: [`DaType::Blob`].
    ///
    /// [`target_num_frames`]: EncoderConfig::target_num_frames
    pub da_type: DaType,

    /// Compression algorithm used for newly opened channels.
    ///
    /// Brotli channels are accepted only after Fjord. Zlib remains valid on both
    /// sides of the fork and should be selected for pre-Fjord environments.
    ///
    /// Default: [`CompressionAlgo::Brotli10`].
    pub compression_algo: CompressionAlgo,

    /// Maximum serialized size of a single L1 calldata transaction in bytes.
    ///
    /// When set, the calldata frame packing path accumulates frames until adding
    /// the next frame would exceed this limit, then cuts the transaction at that point.
    /// At least one frame is always included regardless of size, so oversized frames
    /// are still submitted (governed by [`max_frame_size`] instead).
    ///
    /// This is a no-op when [`da_type`] is [`DaType::Blob`], since the blob size is
    /// the binding constraint for blob DA.
    ///
    /// Default: `None` (no cap).
    ///
    /// [`max_frame_size`]: EncoderConfig::max_frame_size
    /// [`da_type`]: EncoderConfig::da_type
    pub max_l1_tx_size_bytes: Option<usize>,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            target_frame_size: Self::MAX_BLOB_FRAME_SIZE,
            max_frame_size: Self::MAX_BLOB_FRAME_SIZE,
            max_channel_duration: 2,
            sub_safety_margin: 0,
            target_num_frames: 1,
            max_blocks_per_span_batch: None,
            batch_type: BatchType::Single,
            da_type: DaType::Blob,
            compression_algo: CompressionAlgo::Brotli10,
            max_l1_tx_size_bytes: None,
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

    /// Returns the compressed channel bytes that fit in the target frames.
    ///
    /// Frame metadata is reserved in every frame. Brotli's channel-version byte
    /// is reserved once at the start of the first frame. The Span producer uses
    /// this value as its channel-size boundary.
    ///
    /// # Panics
    ///
    /// Panics if the configuration has not passed [`Self::validate`].
    pub fn target_output_size(&self) -> usize {
        let channel_version_size =
            usize::from(!matches!(self.compression_algo, CompressionAlgo::Zlib));
        (self.target_frame_size - Frame::ENCODED_OVERHEAD) * self.target_num_frames
            - channel_version_size
    }

    /// Validate the configuration, returning an error if any constraint is violated.
    ///
    /// This should be called at service startup before constructing a
    /// [`BatchEncoder`](crate::BatchEncoder). Catching misconfigurations early prevents
    /// subtle runtime failures such as channels closing immediately on every
    /// `advance_l1_head` call (which occurs when `sub_safety_margin >= max_channel_duration`).
    pub const fn validate(&self) -> Result<(), EncoderConfigError> {
        if self.sub_safety_margin >= self.max_channel_duration {
            return Err(EncoderConfigError::SafetyMarginTooLarge {
                sub_safety_margin: self.sub_safety_margin,
                max_channel_duration: self.max_channel_duration,
            });
        }

        // Every frame must carry at least one compressed byte in addition to
        // its metadata and the optional Brotli channel-version byte.
        let channel_version_size =
            if matches!(self.compression_algo, CompressionAlgo::Zlib) { 0 } else { 1 };
        let min_frame_size = Frame::ENCODED_OVERHEAD + channel_version_size + 1;

        if self.max_frame_size < min_frame_size {
            return Err(EncoderConfigError::FrameSizeTooSmall {
                max_frame_size: self.max_frame_size,
                min_frame_size,
            });
        }

        if self.target_frame_size < min_frame_size {
            return Err(EncoderConfigError::TargetFrameSizeTooSmall {
                target_frame_size: self.target_frame_size,
                min_frame_size,
            });
        }

        if self.target_num_frames == 0 {
            return Err(EncoderConfigError::TargetNumFramesZero);
        }

        let target_payload_bytes = self.target_frame_size - Frame::ENCODED_OVERHEAD;
        if target_payload_bytes.checked_mul(self.target_num_frames).is_none() {
            return Err(EncoderConfigError::TargetOutputSizeOverflow {
                target_frame_size: self.target_frame_size,
                target_num_frames: self.target_num_frames,
            });
        }

        if matches!(self.da_type, DaType::Calldata) && self.target_num_frames != 1 {
            return Err(EncoderConfigError::CalldataRequiresSingleFrame {
                target_num_frames: self.target_num_frames,
            });
        }

        if matches!(self.max_blocks_per_span_batch, Some(0)) {
            return Err(EncoderConfigError::MaxBlocksPerSpanBatchZero);
        }

        if matches!(self.da_type, DaType::Blob) && self.max_frame_size > Self::MAX_BLOB_FRAME_SIZE {
            return Err(EncoderConfigError::BlobFrameSizeTooLarge {
                max_frame_size: self.max_frame_size,
                max_blob_frame_size: Self::MAX_BLOB_FRAME_SIZE,
            });
        }

        if matches!(self.da_type, DaType::Blob)
            && self.target_frame_size > Self::MAX_BLOB_FRAME_SIZE
        {
            return Err(EncoderConfigError::BlobTargetFrameSizeTooLarge {
                target_frame_size: self.target_frame_size,
                max_blob_frame_size: Self::MAX_BLOB_FRAME_SIZE,
            });
        }

        Ok(())
    }

    /// Validate the configuration against the active rollup state.
    ///
    /// `next_l2_timestamp` should be the timestamp of the next L2 block the
    /// batcher may encode. Span batches and Brotli compression are only valid
    /// once Fjord is active for that next block.
    pub fn validate_for_rollup_config(
        &self,
        rollup_config: &RollupConfig,
        next_l2_timestamp: u64,
    ) -> Result<(), EncoderConfigError> {
        self.validate()?;

        if matches!(self.batch_type, BatchType::Span)
            && !rollup_config.is_fjord_active(next_l2_timestamp)
        {
            return rollup_config.upgrades.fjord_time.map_or(
                Err(EncoderConfigError::SpanBatchRequiresScheduledFjord { next_l2_timestamp }),
                |fjord_time| {
                    Err(EncoderConfigError::SpanBatchBeforeFjord { next_l2_timestamp, fjord_time })
                },
            );
        }

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
    /// `max_frame_size` cannot carry one compressed byte and any required channel version.
    #[error(
        "max_frame_size ({max_frame_size}) must be at least {min_frame_size} bytes \
         to carry frame metadata and payload"
    )]
    FrameSizeTooSmall {
        /// The configured maximum frame size.
        max_frame_size: usize,
        /// The minimum frame size for the configured compression algorithm.
        min_frame_size: usize,
    },
    /// `target_frame_size` cannot carry one compressed byte and any required channel version.
    #[error(
        "target_frame_size ({target_frame_size}) must be at least {min_frame_size} bytes \
         to carry frame metadata and payload"
    )]
    TargetFrameSizeTooSmall {
        /// The configured target frame size.
        target_frame_size: usize,
        /// The minimum frame size for the configured compression algorithm.
        min_frame_size: usize,
    },
    /// `target_num_frames == 0`.
    #[error("target_num_frames must be greater than zero")]
    TargetNumFramesZero,
    /// The total target output size does not fit in a `usize`.
    #[error(
        "target output size overflows usize for target_frame_size {target_frame_size} \
         and target_num_frames {target_num_frames}"
    )]
    TargetOutputSizeOverflow {
        /// The configured target frame size.
        target_frame_size: usize,
        /// The configured target number of frames.
        target_num_frames: usize,
    },
    /// `da_type == DaType::Calldata` but `target_num_frames != 1`.
    ///
    /// Calldata mode submits one frame per L1 transaction. Set
    /// `target_num_frames = 1` when using [`DaType::Calldata`].
    #[error("calldata DA requires target_num_frames == 1, got {target_num_frames}")]
    CalldataRequiresSingleFrame {
        /// The configured target number of frames.
        target_num_frames: usize,
    },
    /// `max_blocks_per_span_batch == 0`.
    #[error("max_blocks_per_span_batch must be greater than zero when set")]
    MaxBlocksPerSpanBatchZero,
    /// `da_type == DaType::Blob` but `max_frame_size` leaves no room for the
    /// derivation-version prefix.
    #[error(
        "blob DA max_frame_size ({max_frame_size}) must be at most \
         {max_blob_frame_size} to leave room for the derivation-version prefix"
    )]
    BlobFrameSizeTooLarge {
        /// The configured maximum frame size.
        max_frame_size: usize,
        /// The maximum frame size that leaves room for the derivation-version prefix.
        max_blob_frame_size: usize,
    },
    /// `da_type == DaType::Blob` but `target_frame_size` leaves no room for the
    /// derivation-version prefix.
    #[error(
        "blob DA target_frame_size ({target_frame_size}) must be at most \
         {max_blob_frame_size} to leave room for the derivation-version prefix"
    )]
    BlobTargetFrameSizeTooLarge {
        /// The configured target frame size.
        target_frame_size: usize,
        /// The maximum frame size that leaves room for the derivation-version prefix.
        max_blob_frame_size: usize,
    },
    /// `batch_type == BatchType::Span` before Fjord activates.
    #[error(
        "span batches require Fjord to be active for the next L2 block; \
         next_l2_timestamp ({next_l2_timestamp}) is before fjord_time ({fjord_time})"
    )]
    SpanBatchBeforeFjord {
        /// The timestamp of the next L2 block the batcher may encode.
        next_l2_timestamp: u64,
        /// The configured Fjord activation timestamp.
        fjord_time: u64,
    },
    /// `batch_type == BatchType::Span` but Fjord is not scheduled.
    #[error(
        "span batches require Fjord to be scheduled and active for the next L2 block; \
         next_l2_timestamp is {next_l2_timestamp}"
    )]
    SpanBatchRequiresScheduledFjord {
        /// The timestamp of the next L2 block the batcher may encode.
        next_l2_timestamp: u64,
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

        assert_eq!(cfg.target_frame_size, EncoderConfig::MAX_BLOB_FRAME_SIZE);
        assert_eq!(cfg.max_frame_size, EncoderConfig::MAX_BLOB_FRAME_SIZE);
        assert_eq!(
            cfg.max_frame_size + EncoderConfig::BLOB_DERIVATION_PREFIX_SIZE,
            EncoderConfig::BLOB_MAX_DATA_SIZE
        );
        assert_eq!(cfg.max_blocks_per_span_batch, None);
    }

    #[rstest]
    #[case(CompressionAlgo::Zlib, 154)]
    #[case(CompressionAlgo::Brotli10, 153)]
    fn target_output_size_reserves_frame_overhead(
        #[case] compression_algo: CompressionAlgo,
        #[case] expected: usize,
    ) {
        let cfg = EncoderConfig {
            target_frame_size: 100,
            target_num_frames: 2,
            compression_algo,
            ..EncoderConfig::default()
        };

        assert_eq!(cfg.target_output_size(), expected);
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
    #[case(CompressionAlgo::Zlib, Frame::ENCODED_OVERHEAD, Frame::ENCODED_OVERHEAD + 1)]
    #[case(CompressionAlgo::Brotli10, Frame::ENCODED_OVERHEAD + 1, Frame::ENCODED_OVERHEAD + 2)]
    fn validate_rejects_frame_without_payload_capacity(
        #[case] compression_algo: CompressionAlgo,
        #[case] max_frame_size: usize,
        #[case] min_frame_size: usize,
    ) {
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
    #[case(CompressionAlgo::Zlib, Frame::ENCODED_OVERHEAD + 1)]
    #[case(CompressionAlgo::Brotli10, Frame::ENCODED_OVERHEAD + 2)]
    fn validate_accepts_frame_with_payload_capacity(
        #[case] compression_algo: CompressionAlgo,
        #[case] max_frame_size: usize,
    ) {
        let cfg = EncoderConfig { compression_algo, max_frame_size, ..EncoderConfig::default() };

        assert!(cfg.validate().is_ok());
    }

    #[rstest]
    #[case(CompressionAlgo::Zlib, Frame::ENCODED_OVERHEAD, Frame::ENCODED_OVERHEAD + 1)]
    #[case(CompressionAlgo::Brotli10, Frame::ENCODED_OVERHEAD + 1, Frame::ENCODED_OVERHEAD + 2)]
    fn validate_rejects_target_without_payload_capacity(
        #[case] compression_algo: CompressionAlgo,
        #[case] target_frame_size: usize,
        #[case] min_frame_size: usize,
    ) {
        let cfg = EncoderConfig { compression_algo, target_frame_size, ..EncoderConfig::default() };

        assert!(matches!(
            cfg.validate().unwrap_err(),
            EncoderConfigError::TargetFrameSizeTooSmall {
                target_frame_size: actual,
                min_frame_size: minimum,
            } if actual == target_frame_size && minimum == min_frame_size
        ));
    }

    #[test]
    fn validate_rejects_zero_target_frames() {
        let cfg = EncoderConfig { target_num_frames: 0, ..EncoderConfig::default() };

        assert!(matches!(cfg.validate().unwrap_err(), EncoderConfigError::TargetNumFramesZero));
    }

    #[test]
    fn validate_rejects_target_output_size_overflow() {
        let cfg = EncoderConfig {
            target_frame_size: usize::MAX,
            target_num_frames: 2,
            ..EncoderConfig::default()
        };

        assert!(matches!(
            cfg.validate().unwrap_err(),
            EncoderConfigError::TargetOutputSizeOverflow { .. }
        ));
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
            EncoderConfigError::BlobFrameSizeTooLarge {
                max_frame_size,
                max_blob_frame_size,
            } if max_frame_size == EncoderConfig::BLOB_MAX_DATA_SIZE
                && max_blob_frame_size == EncoderConfig::MAX_BLOB_FRAME_SIZE
        ));
        assert!(err.to_string().contains("derivation-version prefix"));
    }

    #[test]
    fn validate_rejects_blob_target_frame_size_that_leaves_no_prefix_room() {
        let cfg = EncoderConfig {
            target_frame_size: EncoderConfig::BLOB_MAX_DATA_SIZE,
            ..EncoderConfig::default()
        };

        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::BlobTargetFrameSizeTooLarge {
                target_frame_size,
                max_blob_frame_size,
            } if target_frame_size == EncoderConfig::BLOB_MAX_DATA_SIZE
                && max_blob_frame_size == EncoderConfig::MAX_BLOB_FRAME_SIZE
        ));
        assert!(err.to_string().contains("target_frame_size"));
    }

    #[test]
    fn validate_allows_calldata_frame_size_without_blob_prefix_room() {
        let cfg = EncoderConfig {
            da_type: DaType::Calldata,
            target_frame_size: EncoderConfig::BLOB_MAX_DATA_SIZE,
            max_frame_size: EncoderConfig::BLOB_MAX_DATA_SIZE,
            ..EncoderConfig::default()
        };

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_max_blocks_per_span_batch() {
        let cfg = EncoderConfig { max_blocks_per_span_batch: Some(0), ..EncoderConfig::default() };

        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, EncoderConfigError::MaxBlocksPerSpanBatchZero));
    }

    #[test]
    fn validate_allows_one_block_per_span_batch() {
        let cfg = EncoderConfig { max_blocks_per_span_batch: Some(1), ..EncoderConfig::default() };

        assert!(cfg.validate().is_ok());
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

    #[test]
    fn validate_for_rollup_config_rejects_span_before_fjord() {
        let cfg = EncoderConfig { batch_type: BatchType::Span, ..EncoderConfig::default() };
        let rollup_config = rollup_config_with(2, Some(100));

        let err = cfg.validate_for_rollup_config(&rollup_config, 98).unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::SpanBatchBeforeFjord { next_l2_timestamp: 98, fjord_time: 100 }
        ));
    }

    #[test]
    fn validate_for_rollup_config_allows_span_at_fjord_activation() {
        let cfg = EncoderConfig { batch_type: BatchType::Span, ..EncoderConfig::default() };
        let rollup_config = rollup_config_with(2, Some(100));

        assert!(cfg.validate_for_rollup_config(&rollup_config, 100).is_ok());
    }

    #[test]
    fn validate_for_rollup_config_rejects_unscheduled_fjord_for_span() {
        let cfg = EncoderConfig { batch_type: BatchType::Span, ..EncoderConfig::default() };
        let rollup_config = rollup_config_with(2, None);

        let err = cfg.validate_for_rollup_config(&rollup_config, 2).unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::SpanBatchRequiresScheduledFjord { next_l2_timestamp: 2 }
        ));
    }
}
