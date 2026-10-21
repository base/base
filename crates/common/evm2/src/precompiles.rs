//! Base precompile set selection.

use alloy_primitives::{Address, address};
use base_common_genesis::BaseUpgrade;
use evm2::{
    Evm, Precompiles,
    interpreter::{GasTracker, Message},
    precompiles::{
        P256VERIFY, Precompile, PrecompileHalt, PrecompileId, PrecompileResult, bls12_381, bn254,
    },
};

use crate::{BaseEvmTypes, BaseSpecId};

// Precompile addresses whose Base variants override the stock Ethereum entries.
const BN254_PAIR_ADDRESS: Address = address!("0x0000000000000000000000000000000000000008");
const BLS12_G1_MSM_ADDRESS: Address = address!("0x000000000000000000000000000000000000000c");
const BLS12_G2_MSM_ADDRESS: Address = address!("0x000000000000000000000000000000000000000e");
const BLS12_PAIRING_ADDRESS: Address = address!("0x000000000000000000000000000000000000000f");

// Base bn254 pairing input caps (bytes): Granite introduces the cap, Jovian tightens it.
const BN254_PAIR_GRANITE_MAX: usize = 112_687;
const BN254_PAIR_JOVIAN_MAX: usize = 81_984;

// Base BLS12-381 input caps (bytes): Isthmus introduces them, Jovian tightens them.
const BLS_G1_MSM_ISTHMUS_MAX: usize = 513_760;
const BLS_G1_MSM_JOVIAN_MAX: usize = 288_960;
const BLS_G2_MSM_ISTHMUS_MAX: usize = 488_448;
const BLS_G2_MSM_JOVIAN_MAX: usize = 278_784;
const BLS_PAIRING_ISTHMUS_MAX: usize = 235_008;
const BLS_PAIRING_JOVIAN_MAX: usize = 156_672;

/// Defines an input-capped precompile that halts when the input exceeds `$max` and otherwise
/// delegates to the stock evm2 precompile `$inner`. Mirrors the OP-stack precompile variants that
/// bound calldata size at Granite/Isthmus/Jovian.
macro_rules! capped_precompile {
    ($(#[$attr:meta])* $name:ident, $max:expr, $halt:ident, $inner:path) => {
        $(#[$attr])*
        pub fn $name(
            _evm: &mut Evm<'_, BaseEvmTypes>,
            message: &Message<BaseEvmTypes>,
            gas: &mut GasTracker,
        ) -> PrecompileResult {
            let input = message.input.as_ref();
            if input.len() > $max {
                return Err(PrecompileHalt::$halt.into());
            }
            $inner(input, gas)
        }
    };
}

impl BaseEvmTypes {
    capped_precompile!(
        /// bn254 pairing with the Granite input cap.
        run_bn254_pair_granite,
        BN254_PAIR_GRANITE_MAX,
        Bn254PairLength,
        bn254::pair::run_istanbul
    );
    capped_precompile!(
        /// bn254 pairing with the Jovian input cap.
        run_bn254_pair_jovian,
        BN254_PAIR_JOVIAN_MAX,
        Bn254PairLength,
        bn254::pair::run_istanbul
    );
    capped_precompile!(
        /// BLS12-381 G1 MSM with the Isthmus input cap.
        run_bls_g1_msm_isthmus,
        BLS_G1_MSM_ISTHMUS_MAX,
        Bls12381G1MsmInputLength,
        bls12_381::g1_msm::run
    );
    capped_precompile!(
        /// BLS12-381 G1 MSM with the Jovian input cap.
        run_bls_g1_msm_jovian,
        BLS_G1_MSM_JOVIAN_MAX,
        Bls12381G1MsmInputLength,
        bls12_381::g1_msm::run
    );
    capped_precompile!(
        /// BLS12-381 G2 MSM with the Isthmus input cap.
        run_bls_g2_msm_isthmus,
        BLS_G2_MSM_ISTHMUS_MAX,
        Bls12381G2MsmInputLength,
        bls12_381::g2_msm::run
    );
    capped_precompile!(
        /// BLS12-381 G2 MSM with the Jovian input cap.
        run_bls_g2_msm_jovian,
        BLS_G2_MSM_JOVIAN_MAX,
        Bls12381G2MsmInputLength,
        bls12_381::g2_msm::run
    );
    capped_precompile!(
        /// BLS12-381 pairing with the Isthmus input cap.
        run_bls_pairing_isthmus,
        BLS_PAIRING_ISTHMUS_MAX,
        Bls12381PairingInputLength,
        bls12_381::pairing::run
    );
    capped_precompile!(
        /// BLS12-381 pairing with the Jovian input cap.
        run_bls_pairing_jovian,
        BLS_PAIRING_JOVIAN_MAX,
        Bls12381PairingInputLength,
        bls12_381::pairing::run
    );

    /// Returns the Base precompile set for `spec`.
    ///
    /// Starts from the stock Ethereum set for the mapped EVM2 spec ([`Precompiles::base`]) and
    /// layers the Base-specific differences on top:
    ///
    /// - **Fjord** enables RIP-7212 `P256VERIFY` (secp256r1, `0x100`), ahead of its upstream Osaka
    ///   introduction; from **Azul** (which maps to Osaka) the Osaka-priced variant is already
    ///   installed by [`Precompiles::base`].
    /// - **Granite** caps the bn254 pairing (`0x08`) input; **Jovian** tightens the cap.
    /// - **Isthmus** caps the BLS12-381 G1/G2 MSM and pairing (`0x0c`/`0x0e`/`0x0f`) inputs;
    ///   **Jovian** tightens those caps.
    ///
    /// The Beryl/Cobalt dynamic precompiles (B20 factory, registries, `TxContext`, `NonceManager`)
    /// remain follow-up work.
    pub fn precompiles(spec: BaseSpecId) -> Precompiles<Self> {
        let upgrade = spec.upgrade();
        let at = |fork: BaseUpgrade| (upgrade as u8) >= (fork as u8);
        let mut precompiles = Precompiles::base(spec.into());
        let map = precompiles.as_map_mut();

        if at(BaseUpgrade::Fjord) && !at(BaseUpgrade::Azul) {
            map.insert(P256VERIFY());
        }

        // bn254 pairing input cap: Jovian tightens the Granite cap.
        if at(BaseUpgrade::Jovian) {
            map.insert(Precompile::new(
                BN254_PAIR_ADDRESS,
                PrecompileId::Bn254Pairing,
                Self::run_bn254_pair_jovian,
            ));
        } else if at(BaseUpgrade::Granite) {
            map.insert(Precompile::new(
                BN254_PAIR_ADDRESS,
                PrecompileId::Bn254Pairing,
                Self::run_bn254_pair_granite,
            ));
        }

        // BLS12-381 MSM/pairing input caps: present from Isthmus (Prague BLS), tightened at Jovian.
        if at(BaseUpgrade::Jovian) {
            map.insert(Precompile::new(
                BLS12_G1_MSM_ADDRESS,
                PrecompileId::Bls12G1Msm,
                Self::run_bls_g1_msm_jovian,
            ));
            map.insert(Precompile::new(
                BLS12_G2_MSM_ADDRESS,
                PrecompileId::Bls12G2Msm,
                Self::run_bls_g2_msm_jovian,
            ));
            map.insert(Precompile::new(
                BLS12_PAIRING_ADDRESS,
                PrecompileId::Bls12Pairing,
                Self::run_bls_pairing_jovian,
            ));
        } else if at(BaseUpgrade::Isthmus) {
            map.insert(Precompile::new(
                BLS12_G1_MSM_ADDRESS,
                PrecompileId::Bls12G1Msm,
                Self::run_bls_g1_msm_isthmus,
            ));
            map.insert(Precompile::new(
                BLS12_G2_MSM_ADDRESS,
                PrecompileId::Bls12G2Msm,
                Self::run_bls_g2_msm_isthmus,
            ));
            map.insert(Precompile::new(
                BLS12_PAIRING_ADDRESS,
                PrecompileId::Bls12Pairing,
                Self::run_bls_pairing_isthmus,
            ));
        }

        precompiles
    }
}

#[cfg(test)]
mod tests {
    use evm2::{env::BlockEnv, evm::InMemoryDB};

    use super::*;

    /// The RIP-7212 secp256r1 `P256VERIFY` precompile address.
    const P256_ADDRESS: Address = address!("0x0000000000000000000000000000000000000100");

    /// Builds a minimal EVM at `upgrade` (the capped precompile fns ignore the EVM, but their
    /// signature requires one).
    fn evm(upgrade: BaseUpgrade) -> Evm<'static, BaseEvmTypes> {
        let spec = BaseSpecId::new(upgrade);
        Evm::new(
            spec,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(),
            InMemoryDB::default(),
            Precompiles::base(spec.into()),
        )
    }

    #[test]
    fn p256_absent_before_fjord() {
        let mut precompiles = BaseEvmTypes::precompiles(BaseSpecId::new(BaseUpgrade::Ecotone));
        assert!(!precompiles.as_map_mut().contains(&P256_ADDRESS), "no P256 before Fjord");
    }

    #[test]
    fn p256_present_from_fjord() {
        for upgrade in [BaseUpgrade::Fjord, BaseUpgrade::Granite, BaseUpgrade::Isthmus] {
            let mut precompiles = BaseEvmTypes::precompiles(BaseSpecId::new(upgrade));
            assert!(
                precompiles.as_map_mut().contains(&P256_ADDRESS),
                "P256 present at {upgrade:?}",
            );
        }
    }

    #[test]
    fn p256_present_at_azul_via_osaka() {
        // Azul maps to Osaka, where `Precompiles::base` already installs the Osaka-priced P256.
        let mut precompiles = BaseEvmTypes::precompiles(BaseSpecId::new(BaseUpgrade::Azul));
        assert!(precompiles.as_map_mut().contains(&P256_ADDRESS), "P256 present at Azul");
    }

    /// Builds a message carrying `input` bytes, for driving a precompile.
    fn message(input: Vec<u8>) -> Message<BaseEvmTypes> {
        Message::<BaseEvmTypes> { input: input.into(), ..Default::default() }
    }

    #[test]
    fn bn254_pairing_granite_cap_halts_oversized_input() {
        let mut gas = GasTracker::new(u64::MAX);
        let mut evm = evm(BaseUpgrade::Granite);
        // One byte over the Granite cap halts before doing any pairing work.
        let over = message(vec![0u8; BN254_PAIR_GRANITE_MAX + 1]);
        let err = BaseEvmTypes::run_bn254_pair_granite(&mut evm, &over, &mut gas).unwrap_err();
        assert_eq!(err.as_halt(), Some(&PrecompileHalt::Bn254PairLength));
    }

    #[test]
    fn bn254_pairing_jovian_cap_halts_oversized_input() {
        // The Jovian cap is tighter than Granite: an input between the two caps is allowed under
        // Granite but halts under the Jovian variant.
        let mut gas = GasTracker::new(u64::MAX);
        let mut evm = evm(BaseUpgrade::Jovian);
        let over = message(vec![0u8; BN254_PAIR_JOVIAN_MAX + 1]);
        let err = BaseEvmTypes::run_bn254_pair_jovian(&mut evm, &over, &mut gas).unwrap_err();
        assert_eq!(err.as_halt(), Some(&PrecompileHalt::Bn254PairLength));
    }

    #[test]
    fn bls_pairing_isthmus_cap_halts_oversized_input() {
        let mut gas = GasTracker::new(u64::MAX);
        let mut evm = evm(BaseUpgrade::Isthmus);
        let over = message(vec![0u8; BLS_PAIRING_ISTHMUS_MAX + 1]);
        let err = BaseEvmTypes::run_bls_pairing_isthmus(&mut evm, &over, &mut gas).unwrap_err();
        assert_eq!(err.as_halt(), Some(&PrecompileHalt::Bls12381PairingInputLength));
    }

    #[test]
    fn cap_constants_match_the_revm_reference() {
        // Pin every input cap to the revm `base-common-precompiles` source of truth so the two
        // engines can never silently diverge on the OP-stack calldata-size bounds.
        assert_eq!(BN254_PAIR_GRANITE_MAX, base_common_precompiles::GRANITE_MAX_INPUT_SIZE);
        assert_eq!(BN254_PAIR_JOVIAN_MAX, base_common_precompiles::JOVIAN_MAX_INPUT_SIZE);
        assert_eq!(BLS_G1_MSM_ISTHMUS_MAX, base_common_precompiles::ISTHMUS_G1_MSM_MAX_INPUT_SIZE);
        assert_eq!(BLS_G1_MSM_JOVIAN_MAX, base_common_precompiles::JOVIAN_G1_MSM_MAX_INPUT_SIZE);
        assert_eq!(BLS_G2_MSM_ISTHMUS_MAX, base_common_precompiles::ISTHMUS_G2_MSM_MAX_INPUT_SIZE);
        assert_eq!(BLS_G2_MSM_JOVIAN_MAX, base_common_precompiles::JOVIAN_G2_MSM_MAX_INPUT_SIZE);
        assert_eq!(
            BLS_PAIRING_ISTHMUS_MAX,
            base_common_precompiles::ISTHMUS_PAIRING_MAX_INPUT_SIZE
        );
        assert_eq!(BLS_PAIRING_JOVIAN_MAX, base_common_precompiles::JOVIAN_PAIRING_MAX_INPUT_SIZE);
    }

    #[test]
    fn base_precompiles_install_bn254_and_bls_caps() {
        // Granite installs the bn254 pairing cap; Isthmus additionally installs the BLS caps.
        let mut isthmus = BaseEvmTypes::precompiles(BaseSpecId::new(BaseUpgrade::Isthmus));
        let map = isthmus.as_map_mut();
        for address in
            [BN254_PAIR_ADDRESS, BLS12_G1_MSM_ADDRESS, BLS12_G2_MSM_ADDRESS, BLS12_PAIRING_ADDRESS]
        {
            assert!(map.contains(&address), "expected a precompile at {address}");
        }
    }
}
