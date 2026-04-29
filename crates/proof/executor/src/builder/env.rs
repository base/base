//! Environment utility functions for [`StatelessL2Builder`].

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::{calc_next_block_base_fee, eip1559::BaseFeeParams};
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_primitives::U256;
use base_common_evm::OpSpecId;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_proof_mpt::TrieHinter;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
};

use super::StatelessL2Builder;
use crate::{
    ExecutorError, ExecutorResult, TrieDBProvider,
    util::{
        decode_holocene_eip_1559_params_block_header, decode_jovian_eip_1559_params_block_header,
    },
};

impl<P, H, Evm> StatelessL2Builder<'_, P, H, Evm>
where
    P: TrieDBProvider,
    H: TrieHinter,
    Evm: EvmFactory,
{
    /// Returns the active [`EvmEnv`] for the executor.
    pub(crate) fn evm_env(
        &self,
        spec_id: OpSpecId,
        parent_header: &Header,
        payload_attrs: &BasePayloadAttributes,
        base_fee_params: &BaseFeeParams,
        min_base_fee: u64,
    ) -> ExecutorResult<EvmEnv<OpSpecId>> {
        let block_env = self.prepare_block_env(
            spec_id,
            parent_header,
            payload_attrs,
            base_fee_params,
            min_base_fee,
        )?;
        let cfg_env = self.evm_cfg_env(payload_attrs.payload_attributes.timestamp);
        Ok(EvmEnv::new(cfg_env, block_env))
    }

    /// Returns the active [`CfgEnv`] for the executor.
    pub(crate) fn evm_cfg_env(&self, timestamp: u64) -> CfgEnv<OpSpecId> {
        CfgEnv::new()
            .with_chain_id(self.config.l2_chain_id.id())
            .with_spec_and_mainnet_gas_params(OpSpecId::from_timestamp(self.config, timestamp))
    }

    fn next_block_base_fee(
        &self,
        params: BaseFeeParams,
        parent: &Header,
        min_base_fee: u64,
    ) -> Option<u64> {
        if !self.config.is_jovian_active(parent.timestamp()) {
            return parent.next_block_base_fee(params);
        }

        // Starting from Jovian, we use the maximum of the gas used and the blob gas used to
        // calculate the next base fee.
        let gas_used = if parent.blob_gas_used().unwrap_or_default() > parent.gas_used() {
            parent.blob_gas_used().unwrap_or_default()
        } else {
            parent.gas_used()
        };

        let mut next_block_base_fee = calc_next_block_base_fee(
            gas_used,
            parent.gas_limit(),
            parent.base_fee_per_gas().unwrap_or_default(),
            params,
        );

        // If the next block base fee is less than the min base fee, set it to the min base fee.
        // # Note
        // Before Jovian activation, the min-base-fee is 0 so this is a no-op.
        if next_block_base_fee < min_base_fee {
            next_block_base_fee = min_base_fee;
        }

        Some(next_block_base_fee)
    }

    /// Prepares a [`BlockEnv`] with the given [`BasePayloadAttributes`].
    pub(crate) fn prepare_block_env(
        &self,
        spec_id: OpSpecId,
        parent_header: &Header,
        payload_attrs: &BasePayloadAttributes,
        base_fee_params: &BaseFeeParams,
        min_base_fee: u64,
    ) -> ExecutorResult<BlockEnv> {
        // Base L2 blocks do not have a blob fee market. The canonical sequencer EL
        // (`base-reth-node`) hardcodes `excess_blob_gas: 0, blob_gasprice: 1` for every
        // post-Ecotone block — per spec, BLOBBASEFEE always pushes 1 since L2 processes
        // no blobs. The proof executor must mirror that exactly; re-deriving the value
        // from the parent header via the EIP-4844 formula would diverge from canonical
        // state whenever `parent.blob_gas_used` is non-zero, breaking proof generation.
        let blob_excess_gas_and_price = spec_id
            .is_enabled_in(OpSpecId::ECOTONE)
            .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 });

        let next_block_base_fee = self
            .next_block_base_fee(*base_fee_params, parent_header, min_base_fee)
            .unwrap_or_default();

        Ok(BlockEnv {
            number: U256::from(parent_header.number + 1),
            beneficiary: payload_attrs.payload_attributes.suggested_fee_recipient,
            timestamp: U256::from(payload_attrs.payload_attributes.timestamp),
            gas_limit: payload_attrs.gas_limit.ok_or(ExecutorError::MissingGasLimit)?,
            basefee: next_block_base_fee,
            prevrandao: Some(payload_attrs.payload_attributes.prev_randao),
            blob_excess_gas_and_price,
            ..Default::default()
        })
    }

    /// Returns the active base fee parameters for the parent header.
    /// Returns the min-base-fee as the second element of the tuple.
    ///
    /// ## Note
    /// Before Jovian activation, the min-base-fee is 0.
    pub(crate) fn active_base_fee_params(
        config: &RollupConfig,
        parent_header: &Header,
        payload_timestamp: u64,
    ) -> ExecutorResult<(BaseFeeParams, u64)> {
        match config {
            // After Holocene activation, the base fee parameters are stored in the
            // `extraData` field of the parent header. If Holocene wasn't active in the
            // parent block, the default base fee parameters are used.
            _ if config.is_jovian_active(parent_header.timestamp) => {
                decode_jovian_eip_1559_params_block_header(parent_header)
            }
            _ if config.is_holocene_active(parent_header.timestamp) => {
                decode_holocene_eip_1559_params_block_header(parent_header)
                    .map(|base_fee_params| (base_fee_params, 0))
            }
            // If the next payload attribute timestamp is past canyon activation,
            // use the canyon base fee params from the rollup config.
            _ if config.is_canyon_active(payload_timestamp) => {
                // If the payload attribute timestamp is past canyon activation,
                // use the canyon base fee params from the rollup config.
                Ok((config.chain_op_config.post_canyon_params(), 0))
            }
            _ => {
                // If the next payload attribute timestamp is prior to canyon activation,
                // use the default base fee params from the rollup config.
                Ok((config.chain_op_config.pre_canyon_params(), 0))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_eips::eip1559::BaseFeeParams;
    use alloy_primitives::Sealable;
    use base_common_evm::{BaseEvmFactory, OpSpecId};
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_proof_mpt::NoopTrieHinter;
    use revm::context_interface::block::BlobExcessGasAndPrice;

    use crate::{NoopTrieDBProvider, StatelessL2Builder};

    /// Regression test: BLOBBASEFEE on Base L2 must always observe `1` once Ecotone is active,
    /// regardless of the parent header's `blob_gas_used`. The proof executor must mirror the
    /// canonical sequencer EL hardcoding (`excess_blob_gas: 0, blob_gasprice: 1`); any divergence
    /// causes the TEE Nitro enclave to compute a different state root than the sequencer and
    /// fail proof generation. See the parent `prepare_block_env` for the full context.
    #[test]
    fn blob_excess_gas_and_price_is_constant() {
        // `prepare_block_env` is called with an explicit `OpSpecId`, so the rollup config is
        // only consulted by `next_block_base_fee` for the Jovian gate — defaults are fine.
        let config = RollupConfig::default();

        // Parent header chosen to defeat the OLD buggy formula:
        // - `blob_gas_used = 9_517_140` matches the captured Base Sepolia PoC
        // - `excess_blob_gas` and `base_fee_per_gas` must both be `Some(_)`, otherwise
        //   `next_block_excess_blob_gas` short-circuits to `None` and the regression is masked.
        // Under the OLD formula these inputs yielded `blob_gasprice == 5`.
        let parent = Header {
            number: 100,
            timestamp: 1,
            blob_gas_used: Some(9_517_140),
            excess_blob_gas: Some(0),
            base_fee_per_gas: Some(1_000_000),
            gas_limit: 30_000_000,
            ..Header::default()
        };

        let mut payload_attrs = BasePayloadAttributes::default();
        payload_attrs.payload_attributes.timestamp = 2;
        payload_attrs.gas_limit = Some(30_000_000);

        let builder = StatelessL2Builder::new(
            &config,
            BaseEvmFactory::default(),
            NoopTrieDBProvider,
            NoopTrieHinter,
            parent.clone().seal_slow(),
        );

        let block_env = builder
            .prepare_block_env(
                OpSpecId::ISTHMUS,
                &parent,
                &payload_attrs,
                &BaseFeeParams { max_change_denominator: 250, elasticity_multiplier: 6 },
                0,
            )
            .expect("prepare_block_env");

        assert_eq!(
            block_env.blob_excess_gas_and_price,
            Some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 }),
            "proof executor must mirror sequencer EL: blob_gasprice is always 1 post-Ecotone"
        );
    }
}
