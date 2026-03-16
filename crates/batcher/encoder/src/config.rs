//! Encoder configuration and its validation error type.

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
    /// ensuring channels are closed this many L1 blocks before the configured
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
    /// [`SingleBatch`](base_consensus_genesis::batch::SingleBatch)es
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
}

#[cfg(test)]
mod tests {
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
}
