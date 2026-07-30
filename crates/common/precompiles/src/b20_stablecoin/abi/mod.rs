//! Versioned wire (ABI) surfaces for the stablecoin-specific `IB20Stablecoin` interface. The latest
//! surface is named `IB20Stablecoin` in its `vN` module and re-exported here as both
//! [`IB20Stablecoin`] (canonical) and [`IB20StablecoinV2`]; the frozen Beryl surface is
//! [`IB20StablecoinV1`]. The extension is unchanged so far, so `v2` aliases `v1`.
//!
//! Decoding is version-gated by [`B20Abi`](crate::B20Abi), selected per version by
//! [`StablecoinVersion::abi`](crate::StablecoinVersion).

mod v1;
pub use v1::IB20Stablecoin as IB20StablecoinV1;

mod v2;
pub use v2::{IB20Stablecoin, IB20Stablecoin as IB20StablecoinV2};

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
    use crate::IB20Stablecoin;

    #[test]
    fn stablecoin_call_labels_are_stable() {
        assert_eq!(
            IB20Stablecoin::IB20StablecoinCalls::currency(IB20Stablecoin::currencyCall {})
                .as_label(),
            "precompile-b20-stablecoin-currency"
        );
    }
}
