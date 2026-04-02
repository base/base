#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Generates a `pub fn $name() -> B256` that constructs an [`UpgradeDepositSource`] from the
/// given intent string and returns its source hash. Accepts optional doc attributes.
///
/// [`UpgradeDepositSource`]: base_alloy_consensus::UpgradeDepositSource
macro_rules! upgrade_source_fn {
    ($(#[$attr:meta])* $name:ident, $intent:literal) => {
        $(#[$attr])*
        pub fn $name() -> ::alloy_primitives::B256 {
            ::base_alloy_consensus::UpgradeDepositSource {
                intent: ::alloc::string::String::from($intent),
            }
            .source_hash()
        }
    };
}

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
pub use utils::UpgradeCalldata;

#[cfg(test)]
mod test_utils;
