#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub use base_comp::{CompressionAlgo, CompressionError};

mod submission;
pub use submission::{
    BatchSubmission, BlobPayload, DaType, FrameEncoder, SubmissionId, SubmissionPayload,
};

mod config;
pub use config::{EncoderConfig, EncoderConfigError};

mod composer;
pub use composer::{BatchComposeError, BatchComposer};

mod channel;
pub use channel::{
    ChannelAddOutcome, ChannelCloseReason, ChannelError, ChannelLimit, ChannelRecord,
};

mod egress;
pub use egress::{ArtifactId, ArtifactState, DaArtifact, DaArtifactPayload, DaEgress};

mod pipeline;
pub use pipeline::{BatchPipeline, DerivationReconciliation, ReorgError, StepError, StepResult};

mod encoder;
pub use encoder::BatchEncoder;

mod metrics;
pub use metrics::BatcherMetrics;

pub mod test_utils;
