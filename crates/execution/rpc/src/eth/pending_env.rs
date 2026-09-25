//! Base pending-block environment construction.

use std::sync::Arc;

use alloy_consensus::BlockHeader;
use alloy_eips::eip1898::BlockNumHash;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{
    BlockOverrides,
    state::{AccountOverride, EvmOverrides, StateOverride},
};
use base_common_chains::{BaseUpgrade, Upgrades};
use base_common_consensus::Predeploys;
use base_common_evm::BaseTime;
use base_common_genesis::{ChainGenesis, RollupConfig};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseNextBlockEnvAttributes;
use reth_chainspec::{EthChainSpec, Hardforks};
use reth_errors::RethError;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{NodePrimitives, SealedHeader};
use reth_revm::{
    database::StateProviderDatabase,
    db::{State, states::bundle_state::BundleRetention},
};
use reth_rpc_eth_api::helpers::pending_block::{BuildPendingEnv, PendingEnvBuilder};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::StateProvider;
use revm::Database;

/// Builds Base pending environments on the canonical L2 block schedule.
/// Explicit block overrides retain the generic builder used by `eth_simulateV1`.
#[derive(Debug)]
pub struct BasePendingEnvBuilder {
    chain_spec: Arc<BaseChainSpec>,
}

impl BasePendingEnvBuilder {
    /// Creates a pending environment builder.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Builds the Base schedule using the EL genesis anchor and runtime upgrade configuration.
    pub fn schedule(chain_spec: &(impl EthChainSpec + Hardforks)) -> RollupConfig {
        let genesis = chain_spec.genesis_header();
        let mut schedule = RollupConfig {
            genesis: ChainGenesis {
                l2: BlockNumHash { number: genesis.number(), hash: Default::default() },
                l2_time: genesis.timestamp(),
                ..Default::default()
            },
            // Base's legacy cadence is two seconds; Denim cadence comes from RollupConfig.
            block_time: 2,
            l2_chain_id: chain_spec.chain(),
            ..Default::default()
        };
        if let Some(timestamp) = chain_spec.fork(BaseUpgrade::Denim).as_timestamp() {
            schedule.upgrades.set_activation_timestamp(BaseUpgrade::Denim, timestamp);
        }
        schedule
    }

    /// Applies the successor's `BaseTime` transition beneath user-provided state overrides.
    ///
    /// The state must belong to the same hash-pinned parent used to build the environment.
    /// No changes are persisted, and unknown successor L1 attributes are left at parent values.
    /// User block overrides apply afterward; they do not reschedule the millisecond component.
    pub fn state_overrides(
        chain_spec: &(impl EthChainSpec + Hardforks),
        state: &dyn StateProvider,
        block_number: u64,
        timestamp: u64,
        mut overrides: EvmOverrides,
    ) -> Result<EvmOverrides, EthApiError> {
        let schedule = Self::schedule(chain_spec);
        if !schedule.is_denim_active_at_timestamp(timestamp) {
            return Ok(overrides);
        }

        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(state))
            .with_bundle_update()
            .build();
        BaseTime::ensure_predeploy(&schedule, timestamp, &mut db).map_err(RethError::other)?;
        let current = db.storage(Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT)?;
        let millis = schedule.l2_block_timestamp_parts(block_number).1;
        let value = (current & !U256::from(u16::MAX)) | U256::from(millis);

        db.merge_transitions(BundleRetention::PlainState);
        let mut protocol: StateOverride = db
            .take_bundle()
            .state
            .into_iter()
            .map(|(address, account)| {
                (
                    address,
                    AccountOverride {
                        code: account
                            .info
                            .and_then(|info| info.code)
                            .map(|code| code.original_bytes()),
                        state_diff: Some(
                            account
                                .storage
                                .into_iter()
                                .map(|(slot, value)| {
                                    (B256::from(slot), B256::from(value.present_value))
                                })
                                .collect(),
                        ),
                        ..Default::default()
                    },
                )
            })
            .collect();
        protocol
            .entry(Predeploys::BASE_TIME)
            .or_default()
            .state_diff
            .get_or_insert_default()
            .insert(B256::from(BaseTime::TIMESTAMP_MILLIS_PART_SLOT), B256::from(value));

        let user = overrides.state.get_or_insert_default();
        for (address, account) in protocol {
            let target = user.entry(address).or_default();
            if target.code.is_none() {
                target.code = account.code;
            }
            // A full user state replaces all protocol storage. Leave invalid user state +
            // stateDiff combinations intact so normal RPC validation still rejects them.
            if target.state.is_none() {
                let diff = target.state_diff.get_or_insert_default();
                for (slot, value) in account.state_diff.unwrap_or_default() {
                    diff.entry(slot).or_insert(value);
                }
            }
        }
        Ok(overrides)
    }
}

impl<Evm> PendingEnvBuilder<Evm> for BasePendingEnvBuilder
where
    Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
    <Evm::Primitives as NodePrimitives>::BlockHeader: BlockHeader,
{
    fn pending_env_attributes(
        &self,
        parent: &SealedHeader<<Evm::Primitives as NodePrimitives>::BlockHeader>,
        block_overrides: Option<&BlockOverrides>,
    ) -> Result<BaseNextBlockEnvAttributes, EthApiError> {
        let mut attributes = BaseNextBlockEnvAttributes::build_pending_env(parent, block_overrides);
        if block_overrides.is_none() {
            attributes.timestamp = Self::schedule(self.chain_spec.as_ref())
                .l2_block_timestamp_parts(parent.number().saturating_add(1))
                .0;
        }
        Ok(attributes)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::Address;
    use base_common_consensus::BasePrimitives;
    use base_execution_evm::BaseEvmConfig;
    use reth_chainspec::ForkCondition;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;

    #[test]
    fn successor_schedule_respects_genesis_activation_and_second_boundary() {
        let mut spec = BaseChainSpec::devnet();
        spec.inner.genesis_header =
            SealedHeader::seal_slow(Header { number: 100, timestamp: 11, ..Default::default() });
        // The first scheduled block at/after 14 is block 102 at 15 seconds.
        spec.inner.hardforks.insert(BaseUpgrade::Denim, ForkCondition::Timestamp(14));
        let spec = Arc::new(spec);
        let builder = BasePendingEnvBuilder::new(Arc::clone(&spec));
        let provider = MockEthProvider::<BasePrimitives>::new();
        let upper = U256::from(0x1234) << 128;
        // Preserve a governance-selected implementation and the packed upper storage bits.
        provider.add_account(
            Predeploys::BASE_TIME,
            ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(BaseTime::proxy_bytecode())
                .extend_storage([
                    (B256::from(BaseTime::IMPLEMENTATION_SLOT), U256::from(0x1234)),
                    (B256::ZERO, upper | U256::from(800)),
                ]),
        );

        for (number, parent_time, seconds, millis) in [
            (100, 11, 13, None),
            (101, 13, 15, Some(0)),
            (102, 15, 15, Some(200)),
            (105, 15, 15, Some(800)),
            (106, 15, 16, Some(0)),
        ] {
            let parent = SealedHeader::seal_slow(Header {
                number,
                timestamp: parent_time,
                ..Default::default()
            });
            let attrs = <BasePendingEnvBuilder as PendingEnvBuilder<BaseEvmConfig>>::pending_env_attributes(
                &builder, &parent, None,
            ).unwrap();
            assert_eq!(attrs.timestamp, seconds);
            let overrides = BasePendingEnvBuilder::state_overrides(
                spec.as_ref(),
                &provider,
                number + 1,
                attrs.timestamp,
                EvmOverrides::default(),
            )
            .unwrap();
            if let Some(millis) = millis {
                let state = overrides.state.unwrap();
                assert_eq!(state.len(), 1, "must not redeploy a linked implementation");
                let diff = state[&Predeploys::BASE_TIME].state_diff.as_ref().unwrap();
                assert_eq!(diff.len(), 1);
                assert_eq!(diff[&B256::ZERO], B256::from(upper | U256::from(millis)));
                assert_eq!(
                    provider.storage(Predeploys::BASE_TIME, B256::ZERO).unwrap(),
                    Some(upper | U256::from(800))
                );
            } else {
                assert!(overrides.state.is_none());
            }
        }
    }

    #[test]
    fn full_user_state_replaces_protocol_storage_without_hiding_invalid_overrides() {
        let mut spec = BaseChainSpec::devnet();
        spec.inner.hardforks.insert(BaseUpgrade::Denim, ForkCondition::Timestamp(0));
        let provider = MockEthProvider::<BasePrimitives>::new();
        provider.add_account(
            Predeploys::BASE_TIME,
            ExtendedAccount::new(9, U256::from(17))
                .with_bytecode(BaseTime::proxy_bytecode())
                .extend_storage([(B256::from(BaseTime::IMPLEMENTATION_SLOT), U256::from(0x1234))]),
        );
        let user = StateOverride::from_iter([
            (
                Predeploys::BASE_TIME,
                AccountOverride {
                    state: Some([(B256::ZERO, B256::from(U256::from(333)))].into_iter().collect()),
                    code: Some(alloy_primitives::Bytes::from_static(&[0x00])),
                    nonce: Some(42),
                    ..Default::default()
                },
            ),
            (
                Address::repeat_byte(0x11),
                AccountOverride { balance: Some(U256::from(99)), ..Default::default() },
            ),
        ]);
        let output = BasePendingEnvBuilder::state_overrides(
            &spec,
            &provider,
            1,
            spec.genesis_header().timestamp(),
            EvmOverrides::state(Some(user.clone())),
        )
        .unwrap();
        assert_eq!(output.state.unwrap(), user);

        let mut invalid = user;
        invalid.get_mut(&Predeploys::BASE_TIME).unwrap().state_diff = Some(Default::default());
        let output = BasePendingEnvBuilder::state_overrides(
            &spec,
            &provider,
            1,
            spec.genesis_header().timestamp(),
            EvmOverrides::state(Some(invalid)),
        )
        .unwrap();
        let mut db = State::builder().with_database(StateProviderDatabase::new(&provider)).build();
        assert!(
            alloy_evm::overrides::apply_state_overrides(output.state.unwrap(), &mut db).is_err()
        );
    }
}
