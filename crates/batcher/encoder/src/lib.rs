#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod types;
pub use types::{BatchSubmission, EncoderConfig, ReorgError, StepError, StepResult, SubmissionId};

mod pipeline;
pub use pipeline::BatchPipeline;

mod channel;

mod encoder;
pub use encoder::BatchEncoder;

pub mod test_utils;
