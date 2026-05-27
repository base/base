//! ABI definitions for the stablecoin B-20 variant.
//!
//! [`IB20Stablecoin`] defines only the stablecoin-specific extension.
//! All inherited selectors come from [`crate::IB20`] defined in `b20/abi.rs`.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Stablecoin {
        function currency() external view returns (string);
    }
}

impl IB20Stablecoin::IB20StablecoinCalls {
    /// Returns the stable label for this decoded stablecoin B-20 call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::currency(_) => "precompile-b20-stablecoin-currency",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IB20Stablecoin;

    #[test]
    fn stablecoin_call_labels_are_stable() {
        assert_eq!(
            IB20Stablecoin::IB20StablecoinCalls::currency(IB20Stablecoin::currencyCall {})
                .as_label(),
            "precompile-b20-stablecoin-currency"
        );
    }
}
