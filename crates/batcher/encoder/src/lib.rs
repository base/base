#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod submission;
pub use submission::{BatchSubmission, BatchType, DaType, FrameEncoder, SubmissionId};

mod step;
pub use step::{StepError, StepResult};

mod reorg;
pub use reorg::ReorgError;

mod config;
pub use config::{EncoderConfig, EncoderConfigError};

mod pipeline;
pub use pipeline::BatchPipeline;

mod channel;
pub use channel::{OpenChannel, PendingRef, ReadyChannel};

mod encoder;
pub use encoder::BatchEncoder;

pub mod test_utils;
