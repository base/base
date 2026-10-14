#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cache;
pub use cache::{MAX_GET_TERMS_CACHE_SECS, TermsCache};

mod extension;
pub use extension::{PayerRpcConfig, PayerRpcExtension};

mod filter;
pub use filter::{PayerRequestError, PayerRequestFilter};

mod rpc;
pub use rpc::{PAYER_PRE_EVEREST_RPC_ERROR, PayerApiImpl, PayerApiServer};
