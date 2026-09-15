use std::collections::BTreeMap;

use alloy_consensus::BlockHeader;
use alloy_evm::{
    env::BlockEnvironment,
    overrides::{OverrideBlockHashes, apply_block_overrides, apply_state_overrides},
};
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{
    BlockId, BlockOverrides,
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
};
use base_common_chains::BaseUpgrade;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks, Hardforks};
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, Evm, execute::BlockBuilder};
use reth_primitives_traits::SealedHeader;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_rpc_convert::RpcTxReq;
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, RpcBlock, RpcConvert,
    helpers::{
        Call, EthCall, LoadBlock, SpawnBlocking, estimate::EstimateCall,
        pending_block::LoadPendingBlock,
    },
};
use reth_rpc_eth_types::{
    EthApiError,
    error::{AsEthApiError, FromEthApiError},
    simulate::{self, EthSimulateError},
};
use revm::{context::Block, context_interface::Cfg};
use revm_inspectors::transfer::TransferInspector;

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

impl<N, Rpc> EthCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
    async fn simulate_v1(
        &self,
        payload: SimulatePayload<RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>>,
        block: Option<BlockId>,
    ) -> Result<Vec<SimulatedBlock<RpcBlock<Self::NetworkTypes>>>, Self::Error> {
        if payload.block_state_calls.len() > self.max_simulate_blocks() as usize {
            return Err(EthApiError::other(EthSimulateError::TooManyBlocks).into());
        }

        let block = block.unwrap_or_default();

        let SimulatePayload {
            block_state_calls,
            trace_transfers,
            validation,
            return_full_transactions,
        } = payload;

        if block_state_calls.is_empty() {
            return Err(EthApiError::InvalidParams(String::from("calls are empty.")).into());
        }

        let _permit = self.acquire_owned_blocking_io().await;

        let base_block = self
            .recovered_block(block)
            .await?
            .ok_or_else(|| EthApiError::other(EthSimulateError::BlockNotFound { block }))?;
        let parent = base_block.sealed_header().clone();
        let max_simulate_blocks = self.max_simulate_blocks();

        self.spawn_with_state_at_block(block, move |this, db| {
            let state_provider = db.database.0.0;
            let mut db = State::builder()
                .with_database(StateProviderDatabase::new(&state_provider))
                .with_bundle_update()
                .build();
            let mut parent = parent;

            let chain_spec = this.provider().chain_spec();
            let genesis = chain_spec.genesis_header();
            let denim_timestamp = chain_spec.fork(BaseUpgrade::Denim).as_timestamp();

            // Validate block ordering and fill gaps with empty blocks so every entry has an
            // explicit `number` and `time` override and the chain is contiguous (see the
            // execution-apis spec note: "If the block number is increased more than 1 compared
            // to the previous block, new empty blocks are generated in between.").
            let block_state_calls = sanitize_base_chain(
                block_state_calls,
                &parent,
                genesis.number(),
                genesis.timestamp(),
                denim_timestamp,
                max_simulate_blocks,
            )?;

            let mut blocks: Vec<SimulatedBlock<RpcBlock<Self::NetworkTypes>>> =
                Vec::with_capacity(block_state_calls.len());

            let call_gas_limit = this.call_gas_limit();
            let mut remaining_call_gas_limit = (call_gas_limit > 0).then_some(call_gas_limit);

            for block in block_state_calls {
                let SimBlock { block_overrides, state_overrides, calls } = block;

                let attributes = this
                    .pending_env_builder()
                    .pending_env_attributes(&parent, block_overrides.as_ref())
                    .map_err(Self::Error::from_eth_err)?;

                let mut evm_env = this
                    .evm_config()
                    .next_evm_env(&parent, &attributes)
                    .map_err(RethError::other)
                    .map_err(Self::Error::from_eth_err)?;

                // Always disable EIP-3607
                evm_env.cfg_env.disable_eip3607 = true;

                // EIP-7825's transaction gas cap is only active with Amsterdam's
                // regular/state-gas accounting.
                if !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                    evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                }

                if !validation {
                    // If not explicitly required, we disable nonce check <https://github.com/paradigmxyz/reth/issues/16108>
                    evm_env.cfg_env.disable_nonce_check = true;
                    evm_env.cfg_env.disable_base_fee = true;
                    evm_env.block_env.inner_mut().basefee = 0;
                }

                // Set prevrandao to zero for simulated blocks by default,
                // matching spec behavior where MixDigest is zero-initialized.
                // If user provides an override, it will be applied by apply_block_overrides.
                evm_env.block_env.inner_mut().prevrandao = Some(B256::ZERO);
                if !this
                    .provider()
                    .chain_spec()
                    .is_paris_active_at_block(evm_env.block_env.number().saturating_to())
                {
                    evm_env.block_env.inner_mut().difficulty = parent.difficulty();
                }

                if let Some(block_overrides) = block_overrides {
                    // ensure we don't allow uncapped gas limit per block
                    if let Some(gas_limit_override) = block_overrides.gas_limit
                        && gas_limit_override > evm_env.block_env.gas_limit()
                        && gas_limit_override > this.call_gas_limit()
                    {
                        return Err(EthApiError::other(EthSimulateError::GasLimitReached).into());
                    }
                    apply_block_overrides(block_overrides, &mut db, evm_env.block_env.inner_mut());
                }
                if let Some(ref state_overrides) = state_overrides {
                    apply_state_overrides(state_overrides.clone(), &mut db)
                        .map_err(Self::Error::from_eth_err)?;
                }

                let chain_id = evm_env.cfg_env.chain_id;

                let ctx = this
                    .evm_config()
                    .context_for_next_block(&parent, attributes)
                    .map_err(RethError::other)
                    .map_err(Self::Error::from_eth_err)?;
                let map_err = |e: EthApiError| -> Self::Error {
                    e.as_simulate_error().map_or_else(
                        || Self::Error::from_eth_err(e),
                        |sim_err| Self::Error::from_eth_err(EthApiError::other(sim_err)),
                    )
                };

                let (result, results) = if trace_transfers {
                    // prepare inspector to capture transfer inside the evm so they are recorded
                    // and included in logs
                    let inspector = TransferInspector::new(false).with_logs(true);
                    let evm =
                        this.evm_config().evm_with_env_and_inspector(&mut db, evm_env, inspector);
                    let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                    if let Some(ref state_overrides) = state_overrides {
                        simulate::apply_precompile_overrides(
                            state_overrides,
                            builder.evm_mut().precompiles_mut(),
                        )
                        .map_err(|e| Self::Error::from_eth_err(EthApiError::other(e)))?;
                    }

                    simulate::execute_transactions(
                        builder,
                        &state_provider,
                        calls,
                        &mut remaining_call_gas_limit,
                        chain_id,
                        this.compute_state_root_for_eth_simulate(),
                        this.converter(),
                    )
                    .map_err(map_err)?
                } else {
                    let evm = this.evm_config().evm_with_env(&mut db, evm_env);
                    let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                    if let Some(ref state_overrides) = state_overrides {
                        simulate::apply_precompile_overrides(
                            state_overrides,
                            builder.evm_mut().precompiles_mut(),
                        )
                        .map_err(|e| Self::Error::from_eth_err(EthApiError::other(e)))?;
                    }

                    simulate::execute_transactions(
                        builder,
                        &state_provider,
                        calls,
                        &mut remaining_call_gas_limit,
                        chain_id,
                        this.compute_state_root_for_eth_simulate(),
                        this.converter(),
                    )
                    .map_err(map_err)?
                };

                let simulated_header = result.block.clone_sealed_header();
                db.override_block_hashes(BTreeMap::from([(
                    simulated_header.number(),
                    simulated_header.hash(),
                )]));
                parent = simulated_header;

                let block = simulate::build_simulated_block::<BaseEthApiError, Rpc>(
                    result.block,
                    results,
                    return_full_transactions.into(),
                    this.converter(),
                )?;

                blocks.push(block);
            }

            Ok(blocks)
        })
        .await
    }
}

impl<N, Rpc> EstimateCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> Call for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = N::Evm>,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api.evm_memory_limit()
    }

    #[inline]
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.eth_api.compute_state_root_for_eth_simulate()
    }
}

fn sanitize_base_chain<TxReq, H>(
    blocks: Vec<SimBlock<TxReq>>,
    parent: &SealedHeader<H>,
    genesis_number: u64,
    genesis_timestamp: u64,
    denim_timestamp: Option<u64>,
    max_simulate_blocks: u64,
) -> Result<Vec<SimBlock<TxReq>>, EthApiError>
where
    H: BlockHeader,
{
    const LEGACY_BLOCK_TIME: u64 = 2;
    const DENIM_BLOCK_TIME_MS: u64 = 200;

    let activation_block = denim_timestamp.map(|timestamp| {
        genesis_number
            .saturating_add(timestamp.saturating_sub(genesis_timestamp).div_ceil(LEGACY_BLOCK_TIME))
    });
    let activation_timestamp = activation_block.map(|block| {
        genesis_timestamp
            .saturating_add(block.saturating_sub(genesis_number).saturating_mul(LEGACY_BLOCK_TIME))
    });
    let timestamp_for_block = |block_number: u64| {
        if let (Some(activation_block), Some(activation_timestamp)) =
            (activation_block, activation_timestamp)
            && block_number >= activation_block
        {
            return activation_timestamp.saturating_add(
                block_number.saturating_sub(activation_block).saturating_mul(DENIM_BLOCK_TIME_MS)
                    / 1_000,
            );
        }

        genesis_timestamp.saturating_add(
            block_number.saturating_sub(genesis_number).saturating_mul(LEGACY_BLOCK_TIME),
        )
    };
    let next_timestamp = |block_number: u64, prev_timestamp: u64| {
        let scheduled = timestamp_for_block(block_number);
        if scheduled > prev_timestamp {
            Some(scheduled)
        } else if denim_timestamp.is_some_and(|activation| prev_timestamp >= activation) {
            Some(prev_timestamp)
        } else {
            prev_timestamp.checked_add(LEGACY_BLOCK_TIME)
        }
    };

    let mut out = Vec::with_capacity(blocks.len());
    let base_number = parent.number();
    let mut prev_number = base_number;
    let mut prev_timestamp = parent.timestamp();

    for mut block in blocks {
        let overrides = block.block_overrides.get_or_insert_with(BlockOverrides::default);
        let target_number = if let Some(number) = overrides.number {
            u64::try_from(number).unwrap_or(u64::MAX)
        } else {
            let number = prev_number.saturating_add(1);
            overrides.number = Some(U256::from(number));
            number
        };

        if target_number <= prev_number {
            return Err(EthApiError::other(EthSimulateError::BlockNumberInvalid {
                got: target_number,
                parent: prev_number,
            }));
        }
        if target_number.saturating_sub(base_number) > max_simulate_blocks {
            return Err(EthApiError::other(EthSimulateError::TooManyBlocks));
        }

        let gap = target_number - prev_number;
        if gap > 1 {
            for i in 1..gap {
                let filler_number = prev_number + i;
                let filler_time =
                    next_timestamp(filler_number, prev_timestamp).ok_or_else(|| {
                        EthApiError::other(EthSimulateError::BlockTimestampInvalid {
                            got: prev_timestamp,
                            parent: prev_timestamp,
                        })
                    })?;
                out.push(SimBlock {
                    block_overrides: Some(BlockOverrides {
                        number: Some(U256::from(filler_number)),
                        time: Some(filler_time),
                        ..Default::default()
                    }),
                    state_overrides: None,
                    calls: Vec::new(),
                });
                prev_timestamp = filler_time;
            }
        }

        prev_number = target_number;
        let block_time = if let Some(timestamp) = overrides.time {
            let allows_same_timestamp = denim_timestamp
                .is_some_and(|activation| timestamp >= activation && prev_timestamp >= activation);
            if timestamp < prev_timestamp || (timestamp == prev_timestamp && !allows_same_timestamp)
            {
                return Err(EthApiError::other(EthSimulateError::BlockTimestampInvalid {
                    got: timestamp,
                    parent: prev_timestamp,
                }));
            }
            timestamp
        } else {
            let timestamp = next_timestamp(target_number, prev_timestamp).ok_or_else(|| {
                EthApiError::other(EthSimulateError::BlockTimestampInvalid {
                    got: prev_timestamp,
                    parent: prev_timestamp,
                })
            })?;
            overrides.time = Some(timestamp);
            timestamp
        };
        prev_timestamp = block_time;
        out.push(block);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;

    use super::*;

    #[test]
    fn sanitize_base_chain_uses_denim_schedule() {
        let parent =
            SealedHeader::seal_slow(Header { number: 5, timestamp: 10, ..Default::default() });
        let blocks = vec![SimBlock::<()>::default(), SimBlock::default(), SimBlock::default()];

        let blocks = sanitize_base_chain(blocks, &parent, 0, 0, Some(10), 256).unwrap();
        let timestamps: Vec<_> = blocks
            .iter()
            .map(|block| block.block_overrides.as_ref().unwrap().time.unwrap())
            .collect();

        assert_eq!(timestamps, vec![10, 10, 10]);
    }

    #[test]
    fn sanitize_base_chain_accepts_repeated_timestamp_after_denim() {
        let parent =
            SealedHeader::seal_slow(Header { number: 5, timestamp: 10, ..Default::default() });
        let blocks = vec![SimBlock::<()> {
            block_overrides: Some(BlockOverrides { time: Some(10), ..Default::default() }),
            ..Default::default()
        }];

        assert!(sanitize_base_chain(blocks, &parent, 0, 0, Some(10), 256).is_ok());
    }

    #[test]
    fn sanitize_base_chain_rejects_repeated_timestamp_before_denim() {
        let parent =
            SealedHeader::seal_slow(Header { number: 4, timestamp: 8, ..Default::default() });
        let blocks = vec![SimBlock::<()> {
            block_overrides: Some(BlockOverrides { time: Some(8), ..Default::default() }),
            ..Default::default()
        }];

        assert!(sanitize_base_chain(blocks, &parent, 0, 0, Some(10), 256).is_err());
    }

    #[test]
    fn sanitize_base_chain_preserves_user_timestamp_for_following_default() {
        let parent =
            SealedHeader::seal_slow(Header { number: 5, timestamp: 10, ..Default::default() });
        let blocks = vec![
            SimBlock::<()> {
                block_overrides: Some(BlockOverrides { time: Some(100), ..Default::default() }),
                ..Default::default()
            },
            SimBlock::default(),
        ];

        let blocks = sanitize_base_chain(blocks, &parent, 0, 0, Some(10), 256).unwrap();
        let timestamps: Vec<_> = blocks
            .iter()
            .map(|block| block.block_overrides.as_ref().unwrap().time.unwrap())
            .collect();

        assert_eq!(timestamps, vec![100, 100]);
    }

    #[test]
    fn sanitize_base_chain_preserves_user_timestamp_for_fillers() {
        let parent =
            SealedHeader::seal_slow(Header { number: 5, timestamp: 10, ..Default::default() });
        let blocks = vec![
            SimBlock::<()> {
                block_overrides: Some(BlockOverrides { time: Some(100), ..Default::default() }),
                ..Default::default()
            },
            SimBlock {
                block_overrides: Some(BlockOverrides {
                    number: Some(U256::from(9)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];

        let blocks = sanitize_base_chain(blocks, &parent, 0, 0, Some(10), 256).unwrap();
        let timestamps: Vec<_> = blocks
            .iter()
            .map(|block| block.block_overrides.as_ref().unwrap().time.unwrap())
            .collect();

        assert_eq!(timestamps, vec![100, 100, 100, 100]);
    }
}
