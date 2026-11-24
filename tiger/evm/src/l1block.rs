//! L1 fee parameters read from the `L1Block` predeploy.

use alloy_primitives::U256;
use base_common_consensus::Predeploys;
use base_common_genesis::BaseFeature;
use base_common_l1_fees::L1FeeParams;
use evm2::{
    Evm,
    registry::{HandlerError, HandlerResult},
};

use crate::{BaseEvmTypes, BaseSpecId};

/// Reads the L1 fee parameters that the L1 attributes deposit stores in the `L1Block` predeploy.
///
/// base-reth reads them from the block's state when the first non-deposit transaction is
/// validated, after the deposits before it have run, and keeps them for the rest of the block.
/// The reads do not touch or warm the account, as in the reference, which reads the database
/// directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct L1BlockInfo;

impl L1BlockInfo {
    const L1_BASE_FEE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
    const ECOTONE_L1_FEE_SCALARS_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);
    const L1_OVERHEAD_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);
    const L1_SCALAR_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
    const ECOTONE_L1_BLOB_BASE_FEE_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);
    /// Holds the operator fee scalar and constant (Isthmus) and the DA footprint gas scalar
    /// (Jovian).
    const OPERATOR_FEE_SCALARS_SLOT: U256 = U256::from_limbs([8, 0, 0, 0]);

    const BASE_FEE_SCALAR_OFFSET: usize = 16;
    const BLOB_BASE_FEE_SCALAR_OFFSET: usize = 20;
    const OPERATOR_FEE_SCALAR_OFFSET: usize = 20;
    const OPERATOR_FEE_CONSTANT_OFFSET: usize = 24;
    const DA_FOOTPRINT_GAS_SCALAR_OFFSET: usize = 18;

    fn slot(evm: &mut Evm<'_, BaseEvmTypes>, slot: U256) -> HandlerResult<U256> {
        evm.state_mut()
            .storage_slot_untracked(&Predeploys::L1_BLOCK_INFO, &slot)
            .map_err(HandlerError::Fatal)
    }

    fn bytes(word: U256, offset: usize, len: usize) -> U256 {
        let bytes = word.to_be_bytes::<32>();
        U256::from_be_slice(&bytes[offset..offset + len])
    }

    /// The Jovian DA footprint gas scalar, as stored now.
    pub fn da_footprint_gas_scalar(evm: &mut Evm<'_, BaseEvmTypes>) -> HandlerResult<u64> {
        let word = Self::slot(evm, Self::OPERATOR_FEE_SCALARS_SLOT)?;
        Ok(Self::bytes(word, Self::DA_FOOTPRINT_GAS_SCALAR_OFFSET, 2).to::<u64>())
    }

    /// Reads the parameters for `spec` from the current state.
    pub fn fetch(evm: &mut Evm<'_, BaseEvmTypes>, spec: BaseSpecId) -> HandlerResult<L1FeeParams> {
        let mut params = L1FeeParams {
            l1_base_fee: Self::slot(evm, Self::L1_BASE_FEE_SLOT)?,
            ..L1FeeParams::default()
        };
        if !spec.is_feature_enabled(BaseFeature::BlobAwareL1Fees) {
            params.l1_base_fee_scalar = Self::slot(evm, Self::L1_SCALAR_SLOT)?;
            params.l1_fee_overhead = Some(Self::slot(evm, Self::L1_OVERHEAD_SLOT)?);
            return Ok(params);
        }

        params.l1_blob_base_fee = Some(Self::slot(evm, Self::ECOTONE_L1_BLOB_BASE_FEE_SLOT)?);
        let scalars = Self::slot(evm, Self::ECOTONE_L1_FEE_SCALARS_SLOT)?;
        params.l1_base_fee_scalar = Self::bytes(scalars, Self::BASE_FEE_SCALAR_OFFSET, 4);
        let blob_base_fee_scalar = Self::bytes(scalars, Self::BLOB_BASE_FEE_SCALAR_OFFSET, 4);
        params.l1_blob_base_fee_scalar = Some(blob_base_fee_scalar);
        // Both Ecotone scalars unset: the Bedrock cost function applies, with the overhead.
        params.empty_ecotone_scalars =
            Self::bytes(scalars, Self::BASE_FEE_SCALAR_OFFSET, 8).is_zero();
        if params.empty_ecotone_scalars {
            params.l1_fee_overhead = Some(Self::slot(evm, Self::L1_OVERHEAD_SLOT)?);
        }

        if spec.is_feature_enabled(BaseFeature::OperatorFee) {
            let word = Self::slot(evm, Self::OPERATOR_FEE_SCALARS_SLOT)?;
            params.operator_fee_scalar =
                Some(Self::bytes(word, Self::OPERATOR_FEE_SCALAR_OFFSET, 4));
            params.operator_fee_constant =
                Some(Self::bytes(word, Self::OPERATOR_FEE_CONSTANT_OFFSET, 8));
        }
        if spec.is_feature_enabled(BaseFeature::DaFootprint) {
            params.da_footprint_gas_scalar = Some(U256::from(Self::da_footprint_gas_scalar(evm)?));
        }
        Ok(params)
    }
}
