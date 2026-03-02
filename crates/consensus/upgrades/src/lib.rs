#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod traits;
pub use traits::Hardfork;

mod forks;
pub use forks::Hardforks;

mod fjord;
pub use fjord::Fjord;

mod ecotone;
pub use ecotone::Ecotone;

mod isthmus;
pub use isthmus::Isthmus;

mod jovian;
pub use jovian::Jovian;

mod utils;
pub(crate) use utils::upgrade_to_calldata;

#[cfg(test)]
mod test_utils;
