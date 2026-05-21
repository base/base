//! ABI definitions for the stablecoin B-20 variant.
//!
//! [`IB20Stablecoin`] defines only the stablecoin-specific extension.
//! All inherited selectors come from [`IB20`] re-exported from `b20/abi.rs`.

use alloy_sol_types::sol;

pub use crate::IB20;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Stablecoin {
        function currency() external view returns (string);
    }
}
