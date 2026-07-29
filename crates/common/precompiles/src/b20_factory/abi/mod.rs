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
    use alloc::vec::Vec;

    use alloy_primitives::{Address, B256, b256, keccak256};
    use alloy_sol_types::{SolEnum, SolInterface};

    use super::IB20Factory;
    use crate::B20Variant;

    /// Absolute wire fingerprint for Beryl's (canonical) surface.
    const V1_ABI_FINGERPRINT: B256 =
        b256!("f013318c209df27f732a5153708c89369ee85043970f2f149b0b25ee718bb3c7");

    /// Keccak of sorted call selectors, then sorted event topic0s, then sorted error selectors,
    /// then `B20Variant::COUNT`, then each named variant's ordinal. Order is fixed so a single pin
    /// catches any wire-surface edit.
    ///
    /// The trailing ordinals are this precompile's addition to the shape used by
    /// [`crate::PolicyAbi`]'s fingerprint. A `B20Variant` reorder leaves selectors, topic0s, error
    /// selectors and `COUNT` all untouched — Solidity encodes enums as `uint8`, so no signature
    /// moves — yet it remaps every token address. Hashing the ordinals is what makes the reorder
    /// visible here.
    fn abi_fingerprint(
        selectors: impl IntoIterator<Item = [u8; 4]>,
        event_hashes: impl IntoIterator<Item = B256>,
        error_selectors: impl IntoIterator<Item = [u8; 4]>,
        variant_count: usize,
        variant_ordinals: impl IntoIterator<Item = u8>,
    ) -> B256 {
        let mut selectors: Vec<[u8; 4]> = selectors.into_iter().collect();
        selectors.sort_unstable();

        let mut event_hashes: Vec<B256> = event_hashes.into_iter().collect();
        event_hashes.sort_unstable();

        let mut error_selectors: Vec<[u8; 4]> = error_selectors.into_iter().collect();
        error_selectors.sort_unstable();

        let mut buf = Vec::new();
        for selector in &selectors {
            buf.extend_from_slice(selector);
        }
        for hash in &event_hashes {
            buf.extend_from_slice(hash.as_slice());
        }
        for selector in &error_selectors {
            buf.extend_from_slice(selector);
        }
        buf.push(variant_count as u8);
        buf.extend(variant_ordinals);
        keccak256(&buf)
    }

    fn v1_abi_fingerprint() -> B256 {
        abi_fingerprint(
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
