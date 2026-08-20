#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod submission;
pub use submission::{
    BatchSubmission, BlobPayload, DaType, FrameEncoder, SubmissionId, SubmissionPayload,
};

mod config;
pub use config::{CompressionAlgo, EncoderConfig, EncoderConfigError};

mod pipeline;
pub use pipeline::{BatchPipeline, DerivationReconciliation, ReorgError, StepError, StepResult};

mod channel;
pub use channel::{
    ChannelAddOutcome, ChannelFullReason, FrameState, OpenChannel, OpenChannelError, PendingRef,
    ReadyChannel,
};

#[allow(dead_code, unreachable_pub, unnameable_types)]
mod egress;
#[allow(dead_code, unreachable_pub, unnameable_types)]
mod record;

mod encoder;
pub use encoder::BatchEncoder;

mod metrics;
pub use metrics::BatcherMetrics;

pub mod test_utils;
