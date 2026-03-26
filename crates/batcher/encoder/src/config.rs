//! Encoder configuration and its validation error type.

use base_consensus_genesis::RollupConfig;

use crate::{BatchType, DaType};

/// Configuration for the [`BatchEncoder`](crate::BatchEncoder).
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Target compressed output size per channel. Drives `ShadowCompressor` closure.
    /// Default: 130,044 bytes (`BLOB_MAX_DATA_SIZE`).
    pub target_frame_size: usize,

    /// Maximum byte size of each output frame when draining a closed channel.
    ///
    /// Defaults to `target_frame_size`. Set smaller to force multi-frame output
    /// (e.g. in tests that exercise partial-channel submission and channel timeouts).
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

    /// Number of frames to pack into a single L1 transaction.
    ///
    /// Each frame maps to one EIP-4844 blob, so setting this to N submits N blobs
    /// per transaction. Cancun supports up to 6; Isthmus (EIP-7892) up to 21.
    ///
    /// Default: 1 (one blob per transaction).
    pub target_num_frames: usize,

    /// Whether to encode blocks as individual
    /// [`SingleBatch`](base_protocol::batch::SingleBatch)es
    /// or accumulate them into a single [`SpanBatch`](base_protocol::SpanBatch).
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

    /// Approximate compression ratio used when estimating span batch compressed size.
    ///
    /// Applied as `raw_bytes * approx_compr_ratio` to predict compressed output size
    /// without actually compressing. Should be slightly below the typical observed ratio
    /// to avoid creating a small leftover frame. Also passed to the `ShadowCompressor`
    /// as the ratio hint used when operating in [`BatchType::Single`] mode.
    ///
    /// Default: `0.6` (matches op-batcher's `--approx-compr-ratio` default).
    pub approx_compr_ratio: f64,

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
    /// Default: `None` (no cap; op-batcher equivalent default is 120,000 bytes).
    ///
    /// [`max_frame_size`]: EncoderConfig::max_frame_size
    /// [`da_type`]: EncoderConfig::da_type
    pub max_l1_tx_size_bytes: Option<usize>,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            target_frame_size: 130_044,
            max_frame_size: 130_044,
            max_channel_duration: 2,
            sub_safety_margin: 0,
            target_num_frames: 1,
            batch_type: BatchType::Single,
            da_type: DaType::Blob,
            approx_compr_ratio: 0.6,
            max_l1_tx_size_bytes: None,
        }
    }
}

impl EncoderConfig {
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
        if matches!(self.da_type, DaType::Calldata) && self.target_num_frames != 1 {
            return Err(EncoderConfigError::CalldataRequiresSingleFrame {
                target_num_frames: self.target_num_frames,
            });
        }
        if self.approx_compr_ratio <= 0.0 || self.approx_compr_ratio > 1.0 {
            return Err(EncoderConfigError::InvalidApproxComprRatio {
                approx_compr_ratio: self.approx_compr_ratio,
            });
        }
        Ok(())
    }

    /// Validate the configuration against the active rollup state.
    ///
    /// `next_l2_timestamp` should be the timestamp of the next L2 block the
    /// batcher may encode. Span batches are only valid once Fjord is active for
    /// that next block.
    pub fn validate_for_rollup_config(
        &self,
        rollup_config: &RollupConfig,
        next_l2_timestamp: u64,
    ) -> Result<(), EncoderConfigError> {
        self.validate()?;

        if matches!(self.batch_type, BatchType::Span)
            && !rollup_config.is_fjord_active(next_l2_timestamp)
        {
            return rollup_config.hardforks.fjord_time.map_or(
                Err(EncoderConfigError::SpanBatchRequiresScheduledFjord { next_l2_timestamp }),
                |fjord_time| {
                    Err(EncoderConfigError::SpanBatchBeforeFjord { next_l2_timestamp, fjord_time })
                },
            );
        }

        Ok(())
    }
}

/// Errors returned by [`EncoderConfig::validate`].
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
    /// `da_type == DaType::Calldata` but `target_num_frames != 1`.
    ///
    /// Calldata mode submits one frame per L1 transaction. Set
    /// `target_num_frames = 1` when using [`DaType::Calldata`].
    #[error("calldata DA requires target_num_frames == 1, got {target_num_frames}")]
    CalldataRequiresSingleFrame {
        /// The configured target number of frames.
        target_num_frames: usize,
    },
    /// `approx_compr_ratio <= 0.0 || approx_compr_ratio > 1.0`.
    ///
    /// A ratio ≤ 0.0 causes `compressed_estimate` to always be 0, so the span
    /// accumulator never triggers a size-based channel close, growing unboundedly in
    /// memory until timeout. A ratio > 1.0 overestimates compressed size, causing
    /// channels to close after a single block and producing many tiny span batches.
    ///
    /// Use a value in the range `(0.0, 1.0]`.
    #[error("approx_compr_ratio ({approx_compr_ratio}) must be in the range (0.0, 1.0]")]
    InvalidApproxComprRatio {
        /// The configured approximate compression ratio.
        approx_compr_ratio: f64,
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
}

#[cfg(test)]
mod tests {
    use base_consensus_genesis::HardForkConfig;
    use rstest::rstest;

    use super::*;

    fn config_with(sub_safety_margin: u64, max_channel_duration: u64) -> EncoderConfig {
        EncoderConfig { sub_safety_margin, max_channel_duration, ..EncoderConfig::default() }
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
    #[case(0.1)] // low but valid
    #[case(0.6)] // default
    #[case(1.0)] // exactly 1.0: upper bound is inclusive
    fn validate_approx_compr_ratio_ok(#[case] approx_compr_ratio: f64) {
        let cfg = EncoderConfig { approx_compr_ratio, ..EncoderConfig::default() };
        assert!(cfg.validate().is_ok());
    }

    #[rstest]
    #[case(0.0)] // exactly 0: compressed_estimate always 0
    #[case(-1.0)] // negative: nonsensical ratio
    #[case(1.1)] // above 1: overestimates compressed size
    #[case(f64::INFINITY)]
    fn validate_approx_compr_ratio_err(#[case] approx_compr_ratio: f64) {
        let cfg = EncoderConfig { approx_compr_ratio, ..EncoderConfig::default() };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, EncoderConfigError::InvalidApproxComprRatio { .. }));
        assert!(err.to_string().contains("approx_compr_ratio"));
    }

    fn rollup_config_with(block_time: u64, fjord_time: Option<u64>) -> RollupConfig {
        RollupConfig {
            block_time,
            hardforks: HardForkConfig { fjord_time, ..HardForkConfig::default() },
            ..RollupConfig::default()
        }
    }

    #[test]
    fn validate_for_rollup_config_allows_single_before_fjord() {
        let cfg = EncoderConfig::default();
        let rollup_config = rollup_config_with(2, Some(100));

        assert!(cfg.validate_for_rollup_config(&rollup_config, 98).is_ok());
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
