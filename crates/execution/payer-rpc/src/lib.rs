#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{PayerRpcError, PayerTermsError};

mod terms;
pub use terms::PayerTerms;

mod api;
pub use api::{PayerApiServer, PayerTermsResponse, RateDto, TokenTermsDto};

mod service;
pub use service::PayerApiImpl;
