//! L1 fee parameters read from the `L1Block` predeploy.

use alloy_primitives::{U256, uint};
use base_common_consensus::Predeploys;
use base_common_genesis::BaseUpgrade;
use base_common_l1_fees::L1FeeParams;
use evm2::{Evm, registry::HandlerResult};

use crate::BaseEvmTypes;

/// The `L1Block` predeploy, which stores the L1 attributes posted by each block's L1-info deposit.
///
/// Non-deposit transactions price their L1 data fee, operator fee, and Jovian DA footprint from
/// these attributes. Like the revm reference (`L1BlockInfo::try_fetch`), they are read once per
/// block, at the block's first non-deposit transaction, so they reflect the L1-info deposit that
/// opened the block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct L1Block;

impl L1Block {
    /// The L1 base fee storage slot.
    pub const L1_BASE_FEE_SLOT: U256 = uint!(1_U256);
    /// Stores the Ecotone 32-bit base fee and blob base fee scalars.
    pub const ECOTONE_L1_FEE_SCALARS_SLOT: U256 = uint!(3_U256);
    /// The pre-Ecotone L1 fee overhead storage slot.
    pub const L1_OVERHEAD_SLOT: U256 = uint!(5_U256);
    /// The pre-Ecotone L1 fee scalar storage slot.
    pub const L1_SCALAR_SLOT: U256 = uint!(6_U256);
    /// Stores the Ecotone L1 blob base fee.
    pub const ECOTONE_L1_BLOB_BASE_FEE_SLOT: U256 = uint!(7_U256);
    /// Stores the Isthmus operator fee scalar and constant, and the Jovian DA-footprint gas
    /// scalar.
    pub const OPERATOR_FEE_SCALARS_SLOT: U256 = uint!(8_U256);
    /// Byte offset of the 4-byte base fee scalar in [`Self::ECOTONE_L1_FEE_SCALARS_SLOT`].
    pub const BASE_FEE_SCALAR_OFFSET: usize = 16;
    /// Byte offset of the 4-byte blob base fee scalar in [`Self::ECOTONE_L1_FEE_SCALARS_SLOT`].
    pub const BLOB_BASE_FEE_SCALAR_OFFSET: usize = 20;
    /// Byte offset of the 2-byte DA-footprint gas scalar in [`Self::OPERATOR_FEE_SCALARS_SLOT`].
    pub const DA_FOOTPRINT_GAS_SCALAR_OFFSET: usize = 18;
    /// Byte offset of the 4-byte operator fee scalar in [`Self::OPERATOR_FEE_SCALARS_SLOT`].
    pub const OPERATOR_FEE_SCALAR_OFFSET: usize = 20;
    /// Byte offset of the 8-byte operator fee constant in [`Self::OPERATOR_FEE_SCALARS_SLOT`].
    pub const OPERATOR_FEE_CONSTANT_OFFSET: usize = 24;

    /// Loads the current block's L1 fee parameters into the block environment extension, unless
    /// they were already loaded for this block number.
    pub fn ensure_fee_params(evm: &mut Evm<'_, BaseEvmTypes>) -> HandlerResult<()> {
        let number = evm.block().number;
        if evm.ext().l1_fee_params_block == Some(number) {
            return Ok(());
        }
        let mut block = *evm.block();
        block.ext = Self::fetch_fee_params(evm)?;
        evm.set_block(block);
        evm.ext_mut().l1_fee_params_block = Some(number);
        Ok(())
    }

    /// Reads the L1 fee parameters from the predeploy's accepted state, without marking any
    /// account or slot as accessed by the current transaction.
    pub fn fetch_fee_params(evm: &mut Evm<'_, BaseEvmTypes>) -> HandlerResult<L1FeeParams> {
        let upgrade = evm.config_spec_id().upgrade();
        let at = |fork: BaseUpgrade| (upgrade as u8) >= (fork as u8);
        let mut read =
            |slot: U256| evm.state_mut().storage_slot_untracked(&Predeploys::L1_BLOCK_INFO, &slot);

        let mut params =
            L1FeeParams { l1_base_fee: read(Self::L1_BASE_FEE_SLOT)?, ..Default::default() };
        if !at(BaseUpgrade::Ecotone) {
            params.l1_base_fee_scalar = read(Self::L1_SCALAR_SLOT)?;
            params.l1_fee_overhead = Some(read(Self::L1_OVERHEAD_SLOT)?);
            return Ok(params);
        }

        params.l1_blob_base_fee = Some(read(Self::ECOTONE_L1_BLOB_BASE_FEE_SLOT)?);
        let scalars = read(Self::ECOTONE_L1_FEE_SCALARS_SLOT)?.to_be_bytes::<32>();
        let base_fee_scalar = &scalars[Self::BASE_FEE_SCALAR_OFFSET..][..4];
        let blob_base_fee_scalar = &scalars[Self::BLOB_BASE_FEE_SCALAR_OFFSET..][..4];
        params.l1_base_fee_scalar = U256::from_be_slice(base_fee_scalar);
        params.l1_blob_base_fee_scalar = Some(U256::from_be_slice(blob_base_fee_scalar));
        // Ecotone blocks posted before the scalars were set keep the Bedrock cost function.
        params.empty_ecotone_scalars = base_fee_scalar == [0; 4] && blob_base_fee_scalar == [0; 4];
        if params.empty_ecotone_scalars {
            params.l1_fee_overhead = Some(read(Self::L1_OVERHEAD_SLOT)?);
        }

        if at(BaseUpgrade::Isthmus) {
            let operator = read(Self::OPERATOR_FEE_SCALARS_SLOT)?.to_be_bytes::<32>();
            params.operator_fee_scalar =
                Some(U256::from_be_slice(&operator[Self::OPERATOR_FEE_SCALAR_OFFSET..][..4]));
            params.operator_fee_constant =
                Some(U256::from_be_slice(&operator[Self::OPERATOR_FEE_CONSTANT_OFFSET..][..8]));
        }
        if at(BaseUpgrade::Jovian) {
            let packed = read(Self::OPERATOR_FEE_SCALARS_SLOT)?.to_be_bytes::<32>();
            params.da_footprint_gas_scalar =
                Some(U256::from_be_slice(&packed[Self::DA_FOOTPRINT_GAS_SCALAR_OFFSET..][..2]));
        }
        Ok(params)
    }
}
