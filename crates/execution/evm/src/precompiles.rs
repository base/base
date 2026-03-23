//! Contains Base specific precompiles.
use std::{boxed::Box, string::String};

use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::{
        self, Precompile, PrecompileError, PrecompileId, PrecompileResult, Precompiles, bn254,
        modexp, secp256r1,
    },
    primitives::{Address, OnceLock, hardfork::SpecId},
};

use crate::OpSpecId;

/// Base precompile provider
#[derive(Debug, Clone)]
pub struct BasePrecompiles {
    /// Inner precompile provider is same as Ethereums.
    inner: EthPrecompiles,
    /// Spec id of the precompile provider.
    spec: OpSpecId,
}

impl BasePrecompiles {
    /// Create a new precompile provider with the given `OpSpec`.
    #[inline]
    pub fn new_with_spec(spec: OpSpecId) -> Self {
        let precompiles = match spec {
            spec @ (OpSpecId::BEDROCK
            | OpSpecId::REGOLITH
            | OpSpecId::CANYON
            | OpSpecId::ECOTONE) => Precompiles::new(spec.into_eth_spec().into()),
            OpSpecId::FJORD => Self::fjord(),
            OpSpecId::GRANITE | OpSpecId::HOLOCENE => Self::granite(),
            OpSpecId::ISTHMUS => Self::isthmus(),
            OpSpecId::JOVIAN => Self::jovian(),
            OpSpecId::BASE_V1 => Self::base_v1(),
        };

        Self { inner: EthPrecompiles { precompiles, spec: SpecId::default() }, spec }
    }

    /// Precompiles getter.
    #[inline]
    pub const fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }

    /// Returns precompiles for Fjord spec.
    pub fn fjord() -> &'static Precompiles {
        static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Precompiles::cancun().clone();
            // RIP-7212: secp256r1 P256verify
            precompiles.extend([secp256r1::P256VERIFY]);
            precompiles
        })
    }

    /// Returns precompiles for Granite spec.
    pub fn granite() -> &'static Precompiles {
        static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Self::fjord().clone();
            // Restrict bn254Pairing input size
            precompiles.extend([bn254_pair::GRANITE]);
            precompiles
        })
    }

    /// Returns precompiles for Isthmus spec.
    pub fn isthmus() -> &'static Precompiles {
        static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Self::granite().clone();
            // Prague bls12 precompiles
            precompiles.extend(precompile::bls12_381::precompiles());
            // Isthmus bls12 precompile modifications
            precompiles.extend([
                bls12_381::ISTHMUS_G1_MSM,
                bls12_381::ISTHMUS_G2_MSM,
                bls12_381::ISTHMUS_PAIRING,
            ]);
            precompiles
        })
    }

    /// Returns precompiles for Jovian spec.
    pub fn jovian() -> &'static Precompiles {
        static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Self::isthmus().clone();

            let mut to_remove = Precompiles::default();
            to_remove.extend([
                bn254::pair::ISTANBUL,
                bls12_381::ISTHMUS_G1_MSM,
                bls12_381::ISTHMUS_G2_MSM,
                bls12_381::ISTHMUS_PAIRING,
            ]);

            // Replace the 4 variable-input precompiles with Jovian versions (reduced limits)
            precompiles.difference(&to_remove);

            precompiles.extend([
                bn254_pair::JOVIAN,
                bls12_381::JOVIAN_G1_MSM,
                bls12_381::JOVIAN_G2_MSM,
                bls12_381::JOVIAN_PAIRING,
            ]);

            precompiles
        })
    }

    /// Returns precompiles for the Base V1 spec.
    pub fn base_v1() -> &'static Precompiles {
        static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Self::jovian().clone();

            // Base V1 adopts Osaka pricing and bounds for MODEXP and P256VERIFY.
            precompiles.extend([modexp::OSAKA, secp256r1::P256VERIFY_OSAKA]);

            precompiles
        })
    }
}

impl<CTX> PrecompileProvider<CTX> for BasePrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }
        *self = Self::new_with_spec(spec);
        true
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        self.inner.run(context, inputs)
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.inner.warm_addresses()
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        self.inner.contains(address)
    }
}

impl Default for BasePrecompiles {
    fn default() -> Self {
        Self::new_with_spec(OpSpecId::JOVIAN)
    }
}

/// Bn254 pair precompile.
pub mod bn254_pair {
    use super::{Precompile, PrecompileError, PrecompileId, PrecompileResult, bn254};

    /// Max input size for the bn254 pair precompile.
    pub const GRANITE_MAX_INPUT_SIZE: usize = 112687;
    /// Bn254 pair precompile.
    pub const GRANITE: Precompile =
        Precompile::new(PrecompileId::Bn254Pairing, bn254::pair::ADDRESS, run_pair_granite);

    /// Run the bn254 pair precompile with Base input limit.
    pub fn run_pair_granite(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > GRANITE_MAX_INPUT_SIZE {
            return Err(PrecompileError::Bn254PairLength);
        }
        bn254::run_pair(
            input,
            bn254::pair::ISTANBUL_PAIR_PER_POINT,
            bn254::pair::ISTANBUL_PAIR_BASE,
            gas_limit,
        )
    }

    /// Max input size for the bn254 pair precompile.
    pub const JOVIAN_MAX_INPUT_SIZE: usize = 81_984;
    /// Bn254 pair precompile.
    pub const JOVIAN: Precompile =
        Precompile::new(PrecompileId::Bn254Pairing, bn254::pair::ADDRESS, run_pair_jovian);

    /// Run the bn254 pair precompile with Base input limit.
    pub fn run_pair_jovian(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > JOVIAN_MAX_INPUT_SIZE {
            return Err(PrecompileError::Bn254PairLength);
        }
        bn254::run_pair(
            input,
            bn254::pair::ISTANBUL_PAIR_PER_POINT,
            bn254::pair::ISTANBUL_PAIR_BASE,
            gas_limit,
        )
    }
}

/// `Bls12_381` precompile.
pub mod bls12_381 {
    use revm::precompile::bls12_381_const::{G1_MSM_ADDRESS, G2_MSM_ADDRESS, PAIRING_ADDRESS};

    use super::{Precompile, PrecompileError, PrecompileId, PrecompileResult, precompile};

    /// Max input size for the g1 msm precompile.
    pub const ISTHMUS_G1_MSM_MAX_INPUT_SIZE: usize = 513760;

    /// The maximum input size for the BLS12-381 g1 msm operation after the Jovian Hardfork.
    pub const JOVIAN_G1_MSM_MAX_INPUT_SIZE: usize = 288_960;

    /// Max input size for the g2 msm precompile.
    pub const ISTHMUS_G2_MSM_MAX_INPUT_SIZE: usize = 488448;

    /// Max input size for the g2 msm precompile after the Jovian Hardfork.
    pub const JOVIAN_G2_MSM_MAX_INPUT_SIZE: usize = 278_784;

    /// Max input size for the pairing precompile.
    pub const ISTHMUS_PAIRING_MAX_INPUT_SIZE: usize = 235008;

    /// Max input size for the pairing precompile after the Jovian Hardfork.
    pub const JOVIAN_PAIRING_MAX_INPUT_SIZE: usize = 156_672;

    /// G1 msm precompile.
    pub const ISTHMUS_G1_MSM: Precompile =
        Precompile::new(PrecompileId::Bls12G1Msm, G1_MSM_ADDRESS, run_g1_msm_isthmus);
    /// G2 msm precompile.
    pub const ISTHMUS_G2_MSM: Precompile =
        Precompile::new(PrecompileId::Bls12G2Msm, G2_MSM_ADDRESS, run_g2_msm_isthmus);
    /// Pairing precompile.
    pub const ISTHMUS_PAIRING: Precompile =
        Precompile::new(PrecompileId::Bls12Pairing, PAIRING_ADDRESS, run_pair_isthmus);

    /// G1 msm precompile after the Jovian Hardfork.
    pub const JOVIAN_G1_MSM: Precompile =
        Precompile::new(PrecompileId::Bls12G1Msm, G1_MSM_ADDRESS, run_g1_msm_jovian);
    /// G2 msm precompile after the Jovian Hardfork.
    pub const JOVIAN_G2_MSM: Precompile =
        Precompile::new(PrecompileId::Bls12G2Msm, G2_MSM_ADDRESS, run_g2_msm_jovian);
    /// Pairing precompile after the Jovian Hardfork.
    pub const JOVIAN_PAIRING: Precompile =
        Precompile::new(PrecompileId::Bls12Pairing, PAIRING_ADDRESS, run_pair_jovian);

    /// Run the g1 msm precompile with Base input limit.
    pub fn run_g1_msm_isthmus(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > ISTHMUS_G1_MSM_MAX_INPUT_SIZE {
            return Err(PrecompileError::Other(
                "G1MSM input length too long for Base input size limitation after the Isthmus Hardfork".into(),
            ));
        }
        precompile::bls12_381::g1_msm::g1_msm(input, gas_limit)
    }

    /// Run the g1 msm precompile with Base input limit.
    pub fn run_g1_msm_jovian(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > JOVIAN_G1_MSM_MAX_INPUT_SIZE {
            return Err(PrecompileError::Other(
                "G1MSM input length too long for Base input size limitation after the Jovian Hardfork".into(),
            ));
        }
        precompile::bls12_381::g1_msm::g1_msm(input, gas_limit)
    }

    /// Run the g2 msm precompile with Base input limit.
    pub fn run_g2_msm_isthmus(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > ISTHMUS_G2_MSM_MAX_INPUT_SIZE {
            return Err(PrecompileError::Other(
                "G2MSM input length too long for Base input size limitation".into(),
            ));
        }
        precompile::bls12_381::g2_msm::g2_msm(input, gas_limit)
    }

    /// Run the g2 msm precompile with Base input limit after the Jovian Hardfork.
    pub fn run_g2_msm_jovian(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > JOVIAN_G2_MSM_MAX_INPUT_SIZE {
            return Err(PrecompileError::Other(
                "G2MSM input length too long for Base input size limitation after the Jovian Hardfork".into(),
            ));
        }
        precompile::bls12_381::g2_msm::g2_msm(input, gas_limit)
    }

    /// Run the pairing precompile with Base input limit.
    pub fn run_pair_isthmus(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > ISTHMUS_PAIRING_MAX_INPUT_SIZE {
            return Err(PrecompileError::Other(
                "Pairing input length too long for Base input size limitation".into(),
            ));
        }
        precompile::bls12_381::pairing::pairing(input, gas_limit)
    }

    /// Run the pairing precompile with Base input limit after the Jovian Hardfork.
    pub fn run_pair_jovian(input: &[u8], gas_limit: u64) -> PrecompileResult {
        if input.len() > JOVIAN_PAIRING_MAX_INPUT_SIZE {
            return Err(PrecompileError::Other(
                "Pairing input length too long for Base input size limitation after the Jovian Hardfork".into(),
            ));
        }
        precompile::bls12_381::pairing::pairing(input, gas_limit)
    }
}

#[cfg(test)]
mod tests {
    use std::{vec, vec::Vec};

    use revm::{
        precompile::{PrecompileError, bls12_381_const, modexp, secp256r1},
        primitives::{Bytes, eip7823, hex},
    };

    use super::*;
    use crate::precompiles::bls12_381::{
        ISTHMUS_G1_MSM_MAX_INPUT_SIZE, ISTHMUS_G2_MSM_MAX_INPUT_SIZE,
        ISTHMUS_PAIRING_MAX_INPUT_SIZE, JOVIAN_G1_MSM_MAX_INPUT_SIZE, JOVIAN_G2_MSM_MAX_INPUT_SIZE,
        JOVIAN_PAIRING_MAX_INPUT_SIZE, run_g1_msm_isthmus, run_g1_msm_jovian, run_g2_msm_isthmus,
        run_g2_msm_jovian,
    };

    fn encode_length(len: usize) -> [u8; 32] {
        let mut encoded = [0u8; 32];
        encoded[24..].copy_from_slice(&(len as u64).to_be_bytes());
        encoded
    }

    fn oversized_modexp_input() -> Vec<u8> {
        let mut input = Vec::with_capacity(96);
        input.extend_from_slice(&encode_length(eip7823::INPUT_SIZE_LIMIT + 1));
        input.extend_from_slice(&encode_length(0));
        input.extend_from_slice(&encode_length(1));
        input
    }

    fn modexp_input(base_len: usize, exp_len: usize, mod_len: usize) -> Vec<u8> {
        let mut input = Vec::new();
        input.extend_from_slice(&encode_length(base_len));
        input.extend_from_slice(&encode_length(exp_len));
        input.extend_from_slice(&encode_length(mod_len));
        input.extend(vec![1u8; base_len + exp_len + mod_len]);
        input
    }

    fn assert_jovian_input_limits(spec: OpSpecId) {
        let precompiles = BasePrecompiles::new_with_spec(spec);
        let bn254_pair_precompile = precompiles.precompiles().get(&bn254::pair::ADDRESS).unwrap();

        let mut bad_input_len = bn254_pair::JOVIAN_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bn254_pair::GRANITE_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];

        let res = bn254_pair_precompile.execute(&input, u64::MAX);
        assert!(matches!(res, Err(PrecompileError::Bn254PairLength)));

        let bls12_381_g1_msm_precompile =
            precompiles.precompiles().get(&bls12_381_const::G1_MSM_ADDRESS).unwrap();
        bad_input_len = bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bls12_381::ISTHMUS_G1_MSM_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];
        let res = bls12_381_g1_msm_precompile.execute(&input, u64::MAX);
        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );

        let bls12_381_g2_msm_precompile =
            precompiles.precompiles().get(&bls12_381_const::G2_MSM_ADDRESS).unwrap();
        bad_input_len = bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bls12_381::ISTHMUS_G2_MSM_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];
        let res = bls12_381_g2_msm_precompile.execute(&input, u64::MAX);
        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );

        let bls12_381_pairing_precompile =
            precompiles.precompiles().get(&bls12_381_const::PAIRING_ADDRESS).unwrap();
        bad_input_len = bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bls12_381::ISTHMUS_PAIRING_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];
        let res = bls12_381_pairing_precompile.execute(&input, u64::MAX);
        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }

    #[test]
    fn test_bn254_pair() {
        let input = hex::decode(
            "\
      1c76476f4def4bb94541d57ebba1193381ffa7aa76ada664dd31c16024c43f59\
      3034dd2920f673e204fee2811c678745fc819b55d3e9d294e45c9b03a76aef41\
      209dd15ebff5d46c4bd888e51a93cf99a7329636c63514396b4a452003a35bf7\
      04bf11ca01483bfa8b34b43561848d28905960114c8ac04049af4b6315a41678\
      2bb8324af6cfc93537a2ad1a445cfd0ca2a71acd7ac41fadbf933c2a51be344d\
      120a2a4cf30c1bf9845f20c6fe39e07ea2cce61f0c9bb048165fe5e4de877550\
      111e129f1cf1097710d41c4ac70fcdfa5ba2023c6ff1cbeac322de49d1b6df7c\
      2032c61a830e3c17286de9462bf242fca2883585b93870a73853face6a6bf411\
      198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2\
      1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed\
      090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b\
      12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
        )
        .unwrap();
        let expected =
            hex::decode("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let outcome = bn254_pair::run_pair_granite(&input, 260_000).unwrap();
        assert_eq!(outcome.bytes, expected);

        // Invalid input length
        let input = hex::decode(
            "\
          1111111111111111111111111111111111111111111111111111111111111111\
          1111111111111111111111111111111111111111111111111111111111111111\
          111111111111111111111111111111\
      ",
        )
        .unwrap();

        let res = bn254_pair::run_pair_granite(&input, 260_000);
        assert!(matches!(res, Err(PrecompileError::Bn254PairLength)));

        // Valid input length shorter than 112687
        let input = vec![1u8; 586 * bn254::PAIR_ELEMENT_LEN];
        let res = bn254_pair::run_pair_granite(&input, 260_000);
        assert!(matches!(res, Err(PrecompileError::OutOfGas)));

        // Input length longer than 112687
        let input = vec![1u8; 587 * bn254::PAIR_ELEMENT_LEN];
        let res = bn254_pair::run_pair_granite(&input, 260_000);
        assert!(matches!(res, Err(PrecompileError::Bn254PairLength)));
    }

    #[test]
    fn test_accelerated_bn254_pairing_jovian() {
        const TEST_INPUT: [u8; 384] = hex!(
            "2cf44499d5d27bb186308b7af7af02ac5bc9eeb6a3d147c186b21fb1b76e18da2c0f001f52110ccfe69108924926e45f0b0c868df0e7bde1fe16d3242dc715f61fb19bb476f6b9e44e2a32234da8212f61cd63919354bc06aef31e3cfaff3ebc22606845ff186793914e03e21df544c34ffe2f2f3504de8a79d9159eca2d98d92bd368e28381e8eccb5fa81fc26cf3f048eea9abfdd85d7ed3ab3698d63e4f902fe02e47887507adf0ff1743cbac6ba291e66f59be6bd763950bb16041a0a85e000000000000000000000000000000000000000000000000000000000000000130644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd451971ff0471b09fa93caaf13cbf443c1aede09cc4328f5a62aad45f40ec133eb4091058a3141822985733cbdddfed0fd8d6c104e9e9eff40bf5abfef9ab163bc72a23af9a5ce2ba2796c1f4e453a370eb0af8c212d9dc9acd8fc02c2e907baea223a8eb0b0996252cb548a4487da97b02422ebc0e834613f954de6c7e0afdc1fc"
        );
        const EXPECTED_OUTPUT: [u8; 32] =
            hex!("0000000000000000000000000000000000000000000000000000000000000001");

        let res = bn254_pair::run_pair_jovian(TEST_INPUT.as_ref(), u64::MAX);
        assert!(matches!(res, Ok(outcome) if **outcome.bytes == EXPECTED_OUTPUT));
    }

    #[test]
    fn test_accelerated_bn254_pairing_bad_input_len_jovian() {
        let input = [0u8; bn254_pair::JOVIAN_MAX_INPUT_SIZE + 1];
        let res = bn254_pair::run_pair_jovian(&input, u64::MAX);
        assert!(matches!(res, Err(PrecompileError::Bn254PairLength)));
    }

    #[test]
    fn test_get_jovian_precompile_with_bad_input_len() {
        assert_jovian_input_limits(OpSpecId::JOVIAN);
    }

    #[test]
    fn test_get_base_v1_precompile_with_bad_input_len() {
        assert_jovian_input_limits(OpSpecId::BASE_V1);
    }

    #[test]
    fn test_get_base_v1_precompile_with_osaka_rules() {
        let jovian_precompiles = BasePrecompiles::new_with_spec(OpSpecId::JOVIAN);
        let base_v1_precompiles = BasePrecompiles::new_with_spec(OpSpecId::BASE_V1);

        let jovian_p256 =
            jovian_precompiles.precompiles().get(secp256r1::P256VERIFY.address()).unwrap();
        let base_v1_p256 =
            base_v1_precompiles.precompiles().get(secp256r1::P256VERIFY_OSAKA.address()).unwrap();

        assert!(matches!(
            jovian_p256.execute(&[], 5_000),
            Ok(output) if output.gas_used == secp256r1::P256VERIFY_BASE_GAS_FEE
        ));
        assert!(matches!(base_v1_p256.execute(&[], 5_000), Err(PrecompileError::OutOfGas)));

        let jovian_modexp = jovian_precompiles.precompiles().get(modexp::BERLIN.address()).unwrap();
        let base_v1_modexp =
            base_v1_precompiles.precompiles().get(modexp::OSAKA.address()).unwrap();
        let oversized_input = oversized_modexp_input();

        assert!(jovian_modexp.execute(&oversized_input, u64::MAX).is_ok());
        assert!(matches!(
            base_v1_modexp.execute(&oversized_input, u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
    }

    #[test]
    fn test_cancun_precompiles_in_fjord() {
        // additional to cancun, fjord has p256verify
        assert_eq!(BasePrecompiles::fjord().difference(Precompiles::cancun()).len(), 1)
    }

    #[test]
    fn test_cancun_precompiles_in_granite() {
        // granite has p256verify (fjord)
        // granite has modification of cancun's bn254 pair (doesn't count as new precompile)
        assert_eq!(BasePrecompiles::granite().difference(Precompiles::cancun()).len(), 1)
    }

    #[test]
    fn test_prague_precompiles_in_isthmus() {
        let new_prague_precompiles = Precompiles::prague().difference(Precompiles::cancun());

        // isthmus contains all precompiles that were new in prague, without modifications
        assert!(new_prague_precompiles.difference(BasePrecompiles::isthmus()).is_empty())
    }

    #[test]
    fn test_prague_precompiles_in_jovian() {
        let new_prague_precompiles = Precompiles::prague().difference(Precompiles::cancun());

        // jovian contains all precompiles that were new in prague, without modifications
        assert!(new_prague_precompiles.difference(BasePrecompiles::jovian()).is_empty())
    }

    /// All the addresses of the precompiles in isthmus should be in jovian
    #[test]
    fn test_isthmus_precompiles_in_jovian() {
        let new_isthmus_precompiles = BasePrecompiles::isthmus().difference(Precompiles::cancun());

        // jovian contains all precompiles that were new in isthmus, without modifications
        assert!(new_isthmus_precompiles.difference(BasePrecompiles::jovian()).is_empty())
    }

    #[test]
    fn test_default_precompiles_matches_jovian() {
        let jovian = BasePrecompiles::new_with_spec(OpSpecId::JOVIAN).inner.precompiles;
        let default = BasePrecompiles::default().inner.precompiles;
        assert_eq!(jovian.len(), default.len());

        let intersection = default.intersection(jovian);
        assert_eq!(intersection.len(), jovian.len())
    }

    #[test]
    fn test_g1_isthmus_max_size() {
        let oversized_input = vec![0u8; ISTHMUS_G1_MSM_MAX_INPUT_SIZE + 1];
        let input = Bytes::from(oversized_input);

        let res = run_g1_msm_isthmus(&input, 260_000);

        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }

    #[test]
    fn test_g1_jovian_max_size() {
        let oversized_input = vec![0u8; JOVIAN_G1_MSM_MAX_INPUT_SIZE + 1];
        let input = Bytes::from(oversized_input);

        let res = run_g1_msm_jovian(&input, u64::MAX);

        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }
    #[test]
    fn test_g2_isthmus_max_size() {
        let oversized_input = vec![0u8; ISTHMUS_G2_MSM_MAX_INPUT_SIZE + 1];
        let input = Bytes::from(oversized_input);

        let res = run_g2_msm_isthmus(&input, 260_000);

        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }
    #[test]
    fn test_g2_jovian_max_size() {
        let oversized_input = vec![0u8; JOVIAN_G2_MSM_MAX_INPUT_SIZE + 1];
        let input = Bytes::from(oversized_input);

        let res = run_g2_msm_jovian(&input, u64::MAX);

        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }
    #[test]
    fn test_pair_isthmus_max_size() {
        let oversized_input = vec![0u8; ISTHMUS_PAIRING_MAX_INPUT_SIZE + 1];
        let input = Bytes::from(oversized_input);

        let res = bls12_381::run_pair_isthmus(&input, 260_000);

        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }
    #[test]
    fn test_pair_jovian_max_size() {
        let oversized_input = vec![0u8; JOVIAN_PAIRING_MAX_INPUT_SIZE + 1];
        let input = Bytes::from(oversized_input);

        let res = bls12_381::run_pair_jovian(&input, u64::MAX);

        assert!(
            matches!(res, Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }

    #[test]
    fn test_modexp_eip7823_boundary() {
        let input_ok = modexp_input(eip7823::INPUT_SIZE_LIMIT, 1, 1);
        let result = modexp::osaka_run(&input_ok, u64::MAX);
        assert!(
            !matches!(result, Err(PrecompileError::ModexpEip7823LimitSize)),
            "base_len=1024 should not hit size limit"
        );

        let input_too_large = modexp_input(eip7823::INPUT_SIZE_LIMIT + 1, 1, 1);
        let result = modexp::osaka_run(&input_too_large, u64::MAX);
        assert!(matches!(result, Err(PrecompileError::ModexpEip7823LimitSize)));
    }

    #[test]
    fn test_modexp_eip7823_each_field_rejects() {
        let over = eip7823::INPUT_SIZE_LIMIT + 1;

        let input = modexp_input(over, 0, 1);
        assert!(matches!(
            modexp::osaka_run(&input, u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));

        let input = modexp_input(0, over, 1);
        assert!(matches!(
            modexp::osaka_run(&input, u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));

        let input = modexp_input(0, 0, over);
        assert!(matches!(
            modexp::osaka_run(&input, u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
    }

    #[test]
    fn test_modexp_eip7823_all_fields_at_limit() {
        let limit = eip7823::INPUT_SIZE_LIMIT;
        let input = modexp_input(limit, limit, limit);
        let result = modexp::osaka_run(&input, u64::MAX);
        assert!(
            !matches!(result, Err(PrecompileError::ModexpEip7823LimitSize)),
            "all fields at limit should not trigger size error"
        );
    }

    #[test]
    fn test_modexp_eip7883_min_gas_increase() {
        let input = modexp_input(2, 3, 5);
        let berlin = modexp::berlin_run(&input, u64::MAX).unwrap();
        let osaka = modexp::osaka_run(&input, u64::MAX).unwrap();

        assert!(berlin.gas_used >= 200, "Berlin min gas is 200");
        assert!(osaka.gas_used >= 500, "Osaka min gas is 500");
        assert!(osaka.gas_used > berlin.gas_used, "Osaka gas should exceed Berlin gas");
    }

    #[test]
    fn test_modexp_eip7883_larger_input_gas_increase() {
        let input = modexp_input(32, 32, 32);
        let berlin = modexp::berlin_run(&input, u64::MAX).unwrap();
        let osaka = modexp::osaka_run(&input, u64::MAX).unwrap();
        assert!(osaka.gas_used > berlin.gas_used);
    }

    #[test]
    fn test_p256verify_osaka_exact_gas() {
        let result = secp256r1::p256_verify_osaka(&[], 6_900);
        assert!(matches!(result, Ok(output) if output.gas_used == 6_900));

        let result = secp256r1::p256_verify_osaka(&[], 6_899);
        assert!(matches!(result, Err(PrecompileError::OutOfGas)));
    }

    #[test]
    fn test_p256verify_gas_doubled() {
        assert_eq!(
            secp256r1::P256VERIFY_BASE_GAS_FEE_OSAKA,
            secp256r1::P256VERIFY_BASE_GAS_FEE * 2
        );
    }
}
