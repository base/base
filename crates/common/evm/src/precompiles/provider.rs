use alloc::{boxed::Box, string::String};

use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::{self, Precompiles, bn254, modexp, secp256r1},
    primitives::{Address, OnceLock, hardfork::SpecId},
};

use super::{bls12_381, bn254_pair};
use crate::OpSpecId;

/// Base precompile provider.
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
            OpSpecId::AZUL => Self::azul(),
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

    /// Returns precompiles for the Base Azul spec.
    pub fn azul() -> &'static Precompiles {
        static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Self::jovian().clone();

            // Base Azul adopts Osaka pricing and bounds for MODEXP and P256VERIFY.
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

#[cfg(test)]
mod tests {
    use std::vec;

    use revm::{
        precompile::{PrecompileError, Precompiles, bls12_381_const, bn254, modexp, secp256r1},
        primitives::eip7823,
    };

    use super::*;
    use crate::{
        OpSpecId,
        precompiles::{bls12_381, bn254_pair},
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
        assert!(matches!(
            bn254_pair_precompile.execute(&input, u64::MAX),
            Err(PrecompileError::Bn254PairLength)
        ));

        let g1_msm = precompiles.precompiles().get(&bls12_381_const::G1_MSM_ADDRESS).unwrap();
        bad_input_len = bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bls12_381::ISTHMUS_G1_MSM_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];
        assert!(
            matches!(g1_msm.execute(&input, u64::MAX), Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );

        let g2_msm = precompiles.precompiles().get(&bls12_381_const::G2_MSM_ADDRESS).unwrap();
        bad_input_len = bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bls12_381::ISTHMUS_G2_MSM_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];
        assert!(
            matches!(g2_msm.execute(&input, u64::MAX), Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );

        let pairing = precompiles.precompiles().get(&bls12_381_const::PAIRING_ADDRESS).unwrap();
        bad_input_len = bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE + 1;
        assert!(bad_input_len < bls12_381::ISTHMUS_PAIRING_MAX_INPUT_SIZE);
        let input = vec![0u8; bad_input_len];
        assert!(
            matches!(pairing.execute(&input, u64::MAX), Err(PrecompileError::Other(msg)) if msg.contains("input length too long"))
        );
    }

    #[test]
    fn test_get_jovian_precompile_with_bad_input_len() {
        assert_jovian_input_limits(OpSpecId::JOVIAN);
    }

    #[test]
    fn test_get_azul_precompile_with_bad_input_len() {
        assert_jovian_input_limits(OpSpecId::AZUL);
    }

    #[test]
    fn test_get_azul_precompile_with_osaka_rules() {
        let jovian_precompiles = BasePrecompiles::new_with_spec(OpSpecId::JOVIAN);
        let azul_precompiles = BasePrecompiles::new_with_spec(OpSpecId::AZUL);

        let jovian_p256 =
            jovian_precompiles.precompiles().get(secp256r1::P256VERIFY.address()).unwrap();
        let azul_p256 =
            azul_precompiles.precompiles().get(secp256r1::P256VERIFY_OSAKA.address()).unwrap();

        assert!(matches!(
            jovian_p256.execute(&[], 5_000),
            Ok(output) if output.gas_used == secp256r1::P256VERIFY_BASE_GAS_FEE
        ));
        assert!(matches!(azul_p256.execute(&[], 5_000), Err(PrecompileError::OutOfGas)));

        let jovian_modexp = jovian_precompiles.precompiles().get(modexp::BERLIN.address()).unwrap();
        let azul_modexp = azul_precompiles.precompiles().get(modexp::OSAKA.address()).unwrap();
        let oversized_input = oversized_modexp_input();

        assert!(jovian_modexp.execute(&oversized_input, u64::MAX).is_ok());
        assert!(matches!(
            azul_modexp.execute(&oversized_input, u64::MAX),
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
    fn test_modexp_eip7823_boundary() {
        let input_ok = modexp_input(eip7823::INPUT_SIZE_LIMIT, 1, 1);
        assert!(
            !matches!(
                modexp::osaka_run(&input_ok, u64::MAX),
                Err(PrecompileError::ModexpEip7823LimitSize)
            ),
            "base_len=1024 should not hit size limit"
        );

        let input_too_large = modexp_input(eip7823::INPUT_SIZE_LIMIT + 1, 1, 1);
        assert!(matches!(
            modexp::osaka_run(&input_too_large, u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
    }

    #[test]
    fn test_modexp_eip7823_each_field_rejects() {
        let over = eip7823::INPUT_SIZE_LIMIT + 1;

        assert!(matches!(
            modexp::osaka_run(&modexp_input(over, 0, 1), u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
        assert!(matches!(
            modexp::osaka_run(&modexp_input(0, over, 1), u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
        assert!(matches!(
            modexp::osaka_run(&modexp_input(0, 0, over), u64::MAX),
            Err(PrecompileError::ModexpEip7823LimitSize)
        ));
    }

    #[test]
    fn test_modexp_eip7823_all_fields_at_limit() {
        let limit = eip7823::INPUT_SIZE_LIMIT;
        assert!(
            !matches!(
                modexp::osaka_run(&modexp_input(limit, limit, limit), u64::MAX),
                Err(PrecompileError::ModexpEip7823LimitSize)
            ),
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
        assert!(matches!(
            secp256r1::p256_verify_osaka(&[], 6_900),
            Ok(output) if output.gas_used == 6_900
        ));
        assert!(matches!(secp256r1::p256_verify_osaka(&[], 6_899), Err(PrecompileError::OutOfGas)));
    }

    #[test]
    fn test_p256verify_gas_doubled() {
        assert_eq!(
            secp256r1::P256VERIFY_BASE_GAS_FEE_OSAKA,
            secp256r1::P256VERIFY_BASE_GAS_FEE * 2
        );
    }
}
