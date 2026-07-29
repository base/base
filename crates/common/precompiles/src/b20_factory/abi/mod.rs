//! Wire (ABI) surfaces for the `B20Factory` precompile, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20Factory` in its `vN` module, then re-exported here as
//! both [`IB20Factory`] (canonical) and `IB20FactoryVN`. Older forks keep the same Rust name inside
//! their module so truncated-calldata revert bytes stay stable, and are re-exported as
//! [`IB20FactoryV1`], etc.

mod v1;
pub use v1::{IB20Factory, IB20Factory as IB20FactoryV1};

impl IB20Factory::IB20FactoryCalls {
    /// Returns the stable metric label for this decoded factory call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::createB20(_) => "factory.createB20",
            Self::getB20Address(_) => "factory.getB20Address",
            Self::isB20(_) => "factory.isB20",
            Self::isB20Initialized(_) => "factory.isB20Initialized",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, b256};
    use alloy_sol_types::{SolEnum, SolInterface};

    use super::IB20Factory;
    use crate::{AbiFingerprint, B20Variant};

    /// Absolute wire fingerprint for Beryl's (canonical) surface.
    const V1_ABI_FINGERPRINT: B256 =
        b256!("f013318c209df27f732a5153708c89369ee85043970f2f149b0b25ee718bb3c7");

    /// This surface passes its variant ordinals to [`AbiFingerprint`], unlike the policy registry:
    /// a `B20Variant` reorder leaves selectors, topic0s, error selectors and `COUNT` all untouched
    /// — Solidity encodes enums as `uint8`, so no signature moves — yet it remaps every token
    /// address. Hashing the ordinals is what makes the reorder visible.
    fn v1_abi_fingerprint() -> B256 {
        AbiFingerprint::compute(
            IB20Factory::IB20FactoryCalls::selectors(),
            IB20Factory::IB20FactoryEvents::SELECTORS.iter().copied().map(B256::new),
            IB20Factory::IB20FactoryErrors::selectors(),
            IB20Factory::B20Variant::COUNT,
            [IB20Factory::B20Variant::ASSET as u8, IB20Factory::B20Variant::STABLECOIN as u8],
        )
    }

    #[test]
    fn v1_abi_fingerprint_is_pinned() {
        assert_eq!(v1_abi_fingerprint(), V1_ABI_FINGERPRINT);
    }

    #[test]
    fn every_b20_variant_discriminant_decodes() {
        for discriminant in 0..IB20Factory::B20Variant::COUNT {
            IB20Factory::B20Variant::try_from(discriminant as u8)
                .expect("generated B20Variant discriminant should decode");
        }
    }

    #[test]
    fn factory_call_labels_are_stable() {
        assert_eq!(
            IB20Factory::IB20FactoryCalls::getB20Address(IB20Factory::getB20AddressCall {
                variant: IB20Factory::B20Variant::ASSET,
                sender: Address::ZERO,
                salt: B256::ZERO,
            })
            .as_label(),
            "factory.getB20Address"
        );
    }

    /// The ABI ordinal of each variant *is* byte `[10]` of every token that variant deploys
    /// (`B20Variant::address_prefix`), spliced in as plaintext rather than hashed. Reordering the
    /// `sol!` enum would keep every selector, topic0 and error selector identical while silently
    /// remapping the entire deployed token address space, so the binding is pinned end to end:
    /// ABI ordinal -> Rust discriminant -> derived address -> recovered variant.
    #[test]
    fn abi_ordinals_are_the_address_discriminants() {
        let cases = [
            (IB20Factory::B20Variant::ASSET, B20Variant::Asset, B20Variant::ASSET_DISCRIMINANT),
            (
                IB20Factory::B20Variant::STABLECOIN,
                B20Variant::Stablecoin,
                B20Variant::STABLECOIN_DISCRIMINANT,
            ),
        ];

        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x22);

        for (abi_variant, rust_variant, discriminant) in cases {
            assert_eq!(
                abi_variant as u8, discriminant,
                "ABI ordinal for {rust_variant:?} moved; every token it deploys would remap"
            );
            assert_eq!(B20Variant::from_abi(abi_variant), Some(rust_variant));

            let (address, _) = rust_variant.compute_address(creator, salt);
            assert_eq!(
                address.as_slice()[10],
                abi_variant as u8,
                "address byte [10] must be the ABI ordinal for {rust_variant:?}"
            );
            assert_eq!(
                B20Variant::from_address(address),
                Some(rust_variant),
                "address derived for {rust_variant:?} must recover it"
            );
        }
    }
}
