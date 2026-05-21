//! ABI definitions for the stablecoin B-20 variant.
//!
//! [`IB20Stablecoin`] defines only the stablecoin-specific extension.
//! All inherited selectors come from [`crate::IB20`] defined in `b20/abi.rs`.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Stablecoin {
        /// `currency` is not a recognised ISO 4217 currency code.
        error InvalidCurrency();

        function currency() external view returns (string);
    }
}
