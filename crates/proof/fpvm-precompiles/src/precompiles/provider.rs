//! [`PrecompileProvider`] for FPVM-accelerated Base precompiles.

use alloc::{boxed::Box, string::String, vec, vec::Vec};

use alloy_primitives::{Address, Bytes};
use base_evm::{BasePrecompiles, OpSpecId};
use base_proof_preimage::{HintWriterClient, PreimageOracleClient};
use revm::{
    context::{Cfg, ContextTr, LocalContextTr},
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, Gas, InstructionResult, InterpreterResult},
    precompile::{PrecompileError, PrecompileResult, Precompiles, bls12_381_const, bn254},
    primitives::{hardfork::SpecId, hash_map::HashMap},
};

use super::{ecrecover::ECRECOVER_ADDR, kzg_point_eval::KZG_POINT_EVAL_ADDR};

/// The FPVM-accelerated precompiles.
#[derive(Debug)]
pub struct OpFpvmPrecompiles<H, O> {
    /// The default [`EthPrecompiles`] provider.
    inner: EthPrecompiles,
    /// The accelerated precompiles for the current [`OpSpecId`].
    accelerated_precompiles: HashMap<Address, AcceleratedPrecompileFn<H, O>>,
    /// The [`OpSpecId`] of the precompiles.
    spec: OpSpecId,
    /// The inner [`HintWriterClient`].
    hint_writer: H,
    /// The inner [`PreimageOracleClient`].
    oracle_reader: O,
}

impl<H, O> OpFpvmPrecompiles<H, O>
where
    H: HintWriterClient + Clone + Send + Sync + 'static,
    O: PreimageOracleClient + Clone + Send + Sync + 'static,
{
    #[cfg(test)]
    const fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }

    /// Create a new precompile provider with the given [`OpSpecId`].
    #[inline]
    pub fn new_with_spec(spec: OpSpecId, hint_writer: H, oracle_reader: O) -> Self {
        let precompiles = match spec {
            spec @ (OpSpecId::BEDROCK
            | OpSpecId::REGOLITH
            | OpSpecId::CANYON
            | OpSpecId::ECOTONE) => Precompiles::new(spec.into_eth_spec().into()),
            OpSpecId::FJORD => BasePrecompiles::fjord(),
            OpSpecId::GRANITE | OpSpecId::HOLOCENE => BasePrecompiles::granite(),
            OpSpecId::ISTHMUS => BasePrecompiles::isthmus(),
            OpSpecId::JOVIAN => BasePrecompiles::jovian(),
            OpSpecId::BASE_V1 => BasePrecompiles::base_v1(),
        };

        let accelerated_precompiles = match spec {
            OpSpecId::BEDROCK | OpSpecId::REGOLITH | OpSpecId::CANYON => {
                accelerated_bedrock::<H, O>()
            }
            OpSpecId::ECOTONE | OpSpecId::FJORD => accelerated_ecotone::<H, O>(),
            OpSpecId::GRANITE | OpSpecId::HOLOCENE => accelerated_granite::<H, O>(),
            OpSpecId::ISTHMUS => accelerated_isthmus::<H, O>(),
            OpSpecId::JOVIAN | OpSpecId::BASE_V1 => accelerated_jovian::<H, O>(),
        };

        Self {
            inner: EthPrecompiles { precompiles, spec: SpecId::default() },
            accelerated_precompiles: accelerated_precompiles
                .into_iter()
                .map(|p| (p.address, p.precompile))
                .collect(),
            spec,
            hint_writer,
            oracle_reader,
        }
    }
}

impl<CTX, H, O> PrecompileProvider<CTX> for OpFpvmPrecompiles<H, O>
where
    H: HintWriterClient + Clone + Send + Sync + 'static,
    O: PreimageOracleClient + Clone + Send + Sync + 'static,
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }
        *self = Self::new_with_spec(spec, self.hint_writer.clone(), self.oracle_reader.clone());
        true
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(inputs.gas_limit),
            output: Bytes::new(),
        };

        let input = match &inputs.input {
            revm::interpreter::CallInput::Bytes(bytes) => bytes.clone(),
            revm::interpreter::CallInput::SharedBuffer(range) => context
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|b| Bytes::from(b.to_vec()))
                .unwrap_or_default(),
        };

        // Priority:
        // 1. If the precompile has an accelerated version, use that.
        // 2. If the precompile is not accelerated, use the default version.
        // 3. If the precompile is not found, return None.
        let output =
            if let Some(accelerated) = self.accelerated_precompiles.get(&inputs.bytecode_address) {
                (accelerated)(&input, inputs.gas_limit, &self.hint_writer, &self.oracle_reader)
            } else if let Some(precompile) = self.inner.precompiles.get(&inputs.bytecode_address) {
                precompile.execute(&input, inputs.gas_limit)
            } else {
                return Ok(None);
            };

        match output {
            Ok(output) => {
                let underflow = result.gas.record_cost(output.gas_used);
                assert!(underflow, "Gas underflow is not possible");
                result.result = InstructionResult::Return;
                result.output = output.bytes;
            }
            Err(PrecompileError::Fatal(e)) => return Err(e),
            Err(e) => {
                result.result = if e.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                };
            }
        }

        Ok(Some(result))
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

/// A precompile function that can be accelerated by the FPVM.
type AcceleratedPrecompileFn<H, O> = fn(&[u8], u64, &H, &O) -> PrecompileResult;

/// A tuple type for accelerated precompiles with an associated [`Address`].
struct AcceleratedPrecompile<H, O> {
    /// The address of the precompile.
    address: Address,
    /// The precompile function.
    precompile: AcceleratedPrecompileFn<H, O>,
}

impl<H, O> AcceleratedPrecompile<H, O> {
    /// Create a new accelerated precompile.
    fn new(address: Address, precompile: AcceleratedPrecompileFn<H, O>) -> Self {
        Self { address, precompile }
    }
}

/// The accelerated precompiles for the bedrock spec.
fn accelerated_bedrock<H, O>() -> Vec<AcceleratedPrecompile<H, O>>
where
    H: HintWriterClient + Send + Sync,
    O: PreimageOracleClient + Send + Sync,
{
    vec![
        AcceleratedPrecompile::new(ECRECOVER_ADDR, super::ecrecover::fpvm_ec_recover::<H, O>),
        AcceleratedPrecompile::new(
            bn254::pair::ADDRESS,
            super::bn128_pair::fpvm_bn128_pair::<H, O>,
        ),
    ]
}

/// The accelerated precompiles for the ecotone spec.
fn accelerated_ecotone<H, O>() -> Vec<AcceleratedPrecompile<H, O>>
where
    H: HintWriterClient + Send + Sync,
    O: PreimageOracleClient + Send + Sync,
{
    let mut base = accelerated_bedrock::<H, O>();
    base.push(AcceleratedPrecompile::new(
        KZG_POINT_EVAL_ADDR,
        super::kzg_point_eval::fpvm_kzg_point_eval::<H, O>,
    ));
    base
}

/// The accelerated precompiles for the granite spec.
fn accelerated_granite<H, O>() -> Vec<AcceleratedPrecompile<H, O>>
where
    H: HintWriterClient + Send + Sync,
    O: PreimageOracleClient + Send + Sync,
{
    let mut base = accelerated_ecotone::<H, O>();
    base.push(AcceleratedPrecompile::new(
        bn254::pair::ADDRESS,
        super::bn128_pair::fpvm_bn128_pair_granite::<H, O>,
    ));
    base
}

/// The accelerated precompiles for the isthmus spec.
fn accelerated_isthmus<H, O>() -> Vec<AcceleratedPrecompile<H, O>>
where
    H: HintWriterClient + Send + Sync,
    O: PreimageOracleClient + Send + Sync,
{
    let mut base = accelerated_granite::<H, O>();
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::G1_ADD_ADDRESS,
        super::bls12_g1_add::fpvm_bls12_g1_add::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::G1_MSM_ADDRESS,
        super::bls12_g1_msm::fpvm_bls12_g1_msm::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::G2_ADD_ADDRESS,
        super::bls12_g2_add::fpvm_bls12_g2_add::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::G2_MSM_ADDRESS,
        super::bls12_g2_msm::fpvm_bls12_g2_msm::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::MAP_FP_TO_G1_ADDRESS,
        super::bls12_map_fp::fpvm_bls12_map_fp::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::MAP_FP2_TO_G2_ADDRESS,
        super::bls12_map_fp2::fpvm_bls12_map_fp2::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::PAIRING_ADDRESS,
        super::bls12_pair::fpvm_bls12_pairing::<H, O>,
    ));
    base
}

/// The accelerated precompiles for the jovian spec.
fn accelerated_jovian<H, O>() -> Vec<AcceleratedPrecompile<H, O>>
where
    H: HintWriterClient + Send + Sync,
    O: PreimageOracleClient + Send + Sync,
{
    let mut base = accelerated_isthmus::<H, O>();

    // Replace the 4 variable-input precompiles with Jovian versions (reduced limits)
    base.retain(|p| {
        p.address != bn254::pair::ADDRESS
            && p.address != bls12_381_const::G1_MSM_ADDRESS
            && p.address != bls12_381_const::G2_MSM_ADDRESS
            && p.address != bls12_381_const::PAIRING_ADDRESS
    });

    base.push(AcceleratedPrecompile::new(
        bn254::pair::ADDRESS,
        super::bn128_pair::fpvm_bn128_pair_jovian::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::G1_MSM_ADDRESS,
        super::bls12_g1_msm::fpvm_bls12_g1_msm_jovian::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::G2_MSM_ADDRESS,
        super::bls12_g2_msm::fpvm_bls12_g2_msm_jovian::<H, O>,
    ));
    base.push(AcceleratedPrecompile::new(
        bls12_381_const::PAIRING_ADDRESS,
        super::bls12_pair::fpvm_bls12_pairing_jovian::<H, O>,
    ));

    base
}

#[cfg(test)]
mod tests {
    use base_evm::OpSpecId;
    use base_proof_preimage::{BidirectionalChannel, HintWriter, NativeChannel, OracleReader};
    use revm::precompile::modexp;

    use super::*;

    fn make_hw_or() -> (HintWriter<NativeChannel>, OracleReader<NativeChannel>) {
        let (hint_chan, preimage_chan) =
            (BidirectionalChannel::new().unwrap(), BidirectionalChannel::new().unwrap());
        (HintWriter::new(hint_chan.client), OracleReader::new(preimage_chan.client))
    }

    /// Builds a MODEXP input whose `base_len` exceeds the EIP-7823 limit.
    fn oversized_modexp_input() -> Vec<u8> {
        // EIP-7823 caps each of base_len/exp_len/mod_len at 1024 bytes.
        let over = revm::primitives::eip7823::INPUT_SIZE_LIMIT + 1;
        let encode = |len: usize| -> [u8; 32] {
            let mut b = [0u8; 32];
            b[24..].copy_from_slice(&(len as u64).to_be_bytes());
            b
        };
        let mut input = Vec::with_capacity(96);
        input.extend_from_slice(&encode(over));
        input.extend_from_slice(&encode(0)); // exp_len = 0
        input.extend_from_slice(&encode(1)); // mod_len = 1
        input
    }

    #[test]
    fn test_jovian_and_base_v1_use_different_precompile_sets() {
        let (hw, or_) = make_hw_or();
        let jovian = OpFpvmPrecompiles::new_with_spec(OpSpecId::JOVIAN, hw.clone(), or_.clone());
        let base_v1 = OpFpvmPrecompiles::new_with_spec(OpSpecId::BASE_V1, hw, or_);

        assert!(
            !core::ptr::eq(jovian.precompiles(), base_v1.precompiles()),
            "JOVIAN and BASE_V1 must resolve to different static precompile sets",
        );
    }

    #[test]
    fn test_base_v1_modexp_enforces_eip7823_size_limit() {
        let (hw, or_) = make_hw_or();
        let jovian = OpFpvmPrecompiles::new_with_spec(OpSpecId::JOVIAN, hw.clone(), or_.clone());
        let base_v1 = OpFpvmPrecompiles::new_with_spec(OpSpecId::BASE_V1, hw, or_);

        let modexp_berlin = modexp::BERLIN;
        let addr = modexp_berlin.address();
        let input = oversized_modexp_input();

        assert!(
            jovian.precompiles().get(addr).unwrap().execute(&input, u64::MAX).is_ok(),
            "JOVIAN MODEXP must accept oversized input (Berlin pricing, no EIP-7823 limit)",
        );
        assert!(
            base_v1.precompiles().get(addr).unwrap().execute(&input, u64::MAX).is_err(),
            "BASE_V1 MODEXP must reject oversized input (Osaka pricing, EIP-7823 limit)",
        );
    }
}
