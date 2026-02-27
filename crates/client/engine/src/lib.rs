#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod validator;
pub use validator::{BaseEngineValidatorBuilder, FlashblocksCachedExecutionProvider};
