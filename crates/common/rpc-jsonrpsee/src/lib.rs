#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod traits;
#[cfg(all(feature = "client", feature = "signer"))]
pub use traits::EthSignerApiClient;
#[cfg(feature = "signer")]
pub use traits::EthSignerApiServer;
#[cfg(feature = "client")]
pub use traits::{BaseAdminApiClient, MinerApiExtClient};
pub use traits::{BaseAdminApiServer, MinerApiExtServer};
