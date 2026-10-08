//! Pending block forecasts for the Denim timestamp schedule.

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{
    BlockId,
    state::{AccountOverride, EvmOverrides, StateOverride},
};
use base_common_chains::Upgrades;
use base_common_consensus::Predeploys;
use base_common_evm::{BaseTime, BaseTimeTransitionError};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecError};
use reth_chainspec::ChainSpecProvider;
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, EvmEnvFor};
use reth_rpc_eth_api::{
    FromEthApiError,
    helpers::{LoadState, SpawnBlocking},
};
use reth_rpc_eth_types::{PendingBlockEnv, PendingBlockEnvOrigin};
use revm::{
    Database,
    database::{State, states::bundle_state::BundleRetention},
    database_interface::bal::EvmDatabaseError,
};

use crate::BaseNextBlockEnvAttributes;

/// A pending successor block's scheduled timestamp and millisecond component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BasePendingForecast {
    /// Successor block timestamp in seconds.
    pub timestamp: u64,
    /// Successor block timestamp's millisecond component.
    pub timestamp_millis_part: u16,
}

impl BasePendingForecast {
    /// Returns block `parent_number + 1` on the chain's timestamp schedule, or `None` when the
    /// chain has no Denim schedule.
    pub fn successor(
        chain_spec: &BaseChainSpec,
        parent_number: u64,
    ) -> Result<Option<Self>, BaseChainSpecError> {
        Ok(chain_spec.denim_timestamp_schedule()?.map(|schedule| {
            let (timestamp, timestamp_millis_part) =
                schedule.block_timestamp_parts(parent_number.saturating_add(1));
            Self { timestamp, timestamp_millis_part }
        }))
    }

    /// Resolves an EVM environment like [`LoadState::evm_env_at`], but builds pending state
    /// derived from latest at the scheduled successor timestamp and returns that forecast.
    pub async fn evm_env_at<Eth>(
        eth: &Eth,
        at: BlockId,
    ) -> Result<(EvmEnvFor<Eth::Evm>, BlockId, Option<Self>), Eth::Error>
    where
        Eth: LoadState + SpawnBlocking,
        Eth::Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
    {
        if !at.is_pending() {
            let (env, at) = eth.evm_env_at(at).await?;
            return Ok((env, at, None));
        }

        let PendingBlockEnv { evm_env, origin } = eth.pending_block_env_and_cfg()?;
        let state_at = origin.state_block_id();
        let PendingBlockEnvOrigin::DerivedFromLatest(parent) = origin else {
            return Ok((evm_env, state_at, None));
        };
        let forecast = Self::successor(eth.provider().chain_spec().as_ref(), parent.number())
            .map_err(RethError::other)
            .map_err(Eth::Error::from_eth_err)?;
        let Some(forecast) = forecast else { return Ok((evm_env, state_at, None)) };
        let mut attributes = eth
            .pending_env_builder()
            .pending_env_attributes(&parent, None)
            .map_err(Eth::Error::from_eth_err)?;
        attributes.timestamp = forecast.timestamp;
        let evm_env = eth
            .evm_config()
            .next_evm_env(&parent, &attributes)
            .map_err(RethError::other)
            .map_err(Eth::Error::from_eth_err)?;
        Ok((evm_env, state_at, Some(forecast)))
    }

    /// Adds the forecasted `BaseTime` transition beneath user state overrides. Returns
    /// `overrides` unchanged before Denim.
    ///
    /// User block overrides applied later do not reschedule the millisecond component. Future
    /// L1-origin data remains best effort because this forecast only synthesizes `BaseTime` state.
    pub fn state_overrides<DB: Database>(
        &self,
        chain_spec: &BaseChainSpec,
        db: DB,
        mut overrides: EvmOverrides,
    ) -> Result<EvmOverrides, BaseTimeTransitionError<EvmDatabaseError<DB::Error>>> {
        if !chain_spec.is_denim_active_at_timestamp(self.timestamp) {
            return Ok(overrides);
        }
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        BaseTime::ensure_predeploy(chain_spec, self.timestamp, &mut state)?;
        let packed = state.storage(Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT)?;
        let packed = (packed & !U256::from(u16::MAX)) | U256::from(self.timestamp_millis_part);
        state.merge_transitions(BundleRetention::PlainState);

        let mut synthetic = StateOverride::default();
        let bundle = state.take_bundle();
        for (address, account) in bundle.state {
            let account_override = AccountOverride {
                code: account
                    .info
                    .and_then(|info| {
                        info.code.or_else(|| bundle.contracts.get(&info.code_hash).cloned())
                    })
                    .map(|code| code.original_bytes()),
                state_diff: Some(
                    account
                        .storage
                        .into_iter()
                        .map(|(slot, value)| {
                            (
                                B256::from(slot.to_be_bytes()),
                                B256::from(value.present_value.to_be_bytes()),
                            )
                        })
                        .collect(),
                ),
                ..Default::default()
            };
            synthetic.insert(address, account_override);
        }
        synthetic
            .entry(Predeploys::BASE_TIME)
            .or_default()
            .state_diff
            .get_or_insert_default()
            .insert(
                B256::from(BaseTime::TIMESTAMP_MILLIS_PART_SLOT.to_be_bytes()),
                B256::from(packed.to_be_bytes()),
            );

        let user = overrides.state.get_or_insert_default();
        for (address, generated) in synthetic {
            let existing = user.entry(address).or_default();
            if existing.code.is_none() {
                existing.code = generated.code;
            }
            if existing.state.is_none() {
                let existing_diff = existing.state_diff.get_or_insert_default();
                for (slot, value) in generated.state_diff.unwrap_or_default() {
                    existing_diff.entry(slot).or_insert(value);
                }
            }
        }
        Ok(overrides)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_evm::overrides::apply_state_overrides;
    use alloy_primitives::{Address, Bytes};
    use base_common_chains::BaseUpgrade;
    use reth_chainspec::{ForkCondition, Hardfork};
    use reth_primitives_traits::SealedHeader;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    use super::*;

    const FORECAST: BasePendingForecast =
        BasePendingForecast { timestamp: 1, timestamp_millis_part: 400 };
    const SLOT: U256 = BaseTime::TIMESTAMP_MILLIS_PART_SLOT;

    fn denim_spec(activation: u64) -> BaseChainSpec {
        let mut spec = BaseChainSpec::devnet();
        spec.inner
            .hardforks
            .insert(BaseUpgrade::Denim.boxed(), ForkCondition::Timestamp(activation));
        spec
    }

    /// Returns a canonical `BaseTime` proxy with the given implementation link and slot 0.
    fn base_time_db(implementation: Option<Address>, packed: U256) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        let proxy = AccountInfo::from_bytecode(Bytecode::new_raw(BaseTime::proxy_bytecode()));
        db.insert_account_info(Predeploys::BASE_TIME, proxy);
        let admin = U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice());
        let link = implementation.map(|address| U256::from_be_slice(address.as_slice()));
        for (slot, value) in [
            (BaseTime::ADMIN_SLOT, admin),
            (SLOT, packed),
            (BaseTime::IMPLEMENTATION_SLOT, link.unwrap_or_default()),
        ] {
            db.insert_account_storage(Predeploys::BASE_TIME, slot, value).unwrap();
        }
        db
    }

    fn apply(db: &mut InMemoryDB, overrides: EvmOverrides) -> State<&mut InMemoryDB> {
        let mut state = State::builder().with_database(db).build();
        apply_state_overrides(overrides.state.unwrap(), &mut state).unwrap();
        state
    }

    fn link(state: &mut State<&mut InMemoryDB>) -> U256 {
        state.storage(Predeploys::BASE_TIME, BaseTime::IMPLEMENTATION_SLOT).unwrap()
    }

    #[test]
    fn successor_follows_the_runtime_schedule() {
        let mut spec = BaseChainSpec::devnet();
        assert_eq!(BasePendingForecast::successor(&spec, 0).unwrap(), None);

        spec.inner.genesis_header =
            SealedHeader::seal_slow(Header { number: 100, timestamp: 11, ..Default::default() });
        spec.block_time = Some(3);
        // The first legacy slot at or after 15s is block 102 at 17s.
        spec.inner.hardforks.insert(BaseUpgrade::Denim.boxed(), ForkCondition::Timestamp(15));
        for (parent, timestamp, timestamp_millis_part) in
            [(100, 14, 0), (101, 17, 0), (105, 17, 800), (106, 18, 0)]
        {
            assert_eq!(
                BasePendingForecast::successor(&spec, parent).unwrap(),
                Some(BasePendingForecast { timestamp, timestamp_millis_part }),
                "successor of {parent}"
            );
        }

        spec.block_time = None;
        assert!(BasePendingForecast::successor(&spec, 0).is_err());
    }

    #[test]
    fn state_overrides_apply_the_base_time_transition() {
        // An existing (governance-selected) link and the packed upper bits survive.
        let implementation = Address::repeat_byte(0x12);
        let upper = U256::from(0x1234) << 128;
        let mut db = base_time_db(Some(implementation), upper | U256::from(800));
        let overrides =
            FORECAST.state_overrides(&denim_spec(0), &mut db, EvmOverrides::default()).unwrap();
        assert_eq!(db.storage(Predeploys::BASE_TIME, SLOT).unwrap(), upper | U256::from(800));
        let mut state = apply(&mut db, overrides);
        assert_eq!(state.storage(Predeploys::BASE_TIME, SLOT).unwrap(), upper | U256::from(400));
        assert_eq!(link(&mut state), U256::from_be_slice(implementation.as_slice()));
        assert!(state.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().is_none());

        // An unlinked proxy is linked to the canonical implementation.
        let mut db = base_time_db(None, U256::ZERO);
        let overrides =
            FORECAST.state_overrides(&denim_spec(0), &mut db, EvmOverrides::default()).unwrap();
        let mut state = apply(&mut db, overrides);
        assert_eq!(BaseTime::fetch_timestamp_millis_part(&mut state).unwrap(), 400);
        assert_eq!(
            link(&mut state),
            U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice())
        );
        assert_eq!(
            state.basic(BaseTime::IMPLEMENTATION_ADDRESS).unwrap().unwrap().code_hash,
            BaseTime::IMPLEMENTATION_CODE_HASH
        );

        // Nothing changes before Denim.
        let overrides =
            FORECAST.state_overrides(&denim_spec(2), &mut db, EvmOverrides::default()).unwrap();
        assert!(overrides.state.is_none());
    }

    #[test]
    fn user_state_overrides_take_precedence() {
        let spec = denim_spec(0);
        let slot = B256::from(SLOT.to_be_bytes());
        let user = |account: AccountOverride| {
            EvmOverrides::state(Some(StateOverride::from_iter([(Predeploys::BASE_TIME, account)])))
        };

        // A user slot wins over the forecast; other forecast slots still apply.
        let mut db = base_time_db(None, U256::ZERO);
        let diff = AccountOverride {
            state_diff: Some([(slot, B256::from(U256::from(300)))].into_iter().collect()),
            ..Default::default()
        };
        let overrides = FORECAST.state_overrides(&spec, &mut db, user(diff)).unwrap();
        let mut state = apply(&mut db, overrides);
        assert_eq!(BaseTime::fetch_timestamp_millis_part(&mut state).unwrap(), 300);
        assert_eq!(
            link(&mut state),
            U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice())
        );

        // Full user state and code replace the forecast for that account.
        let full = AccountOverride {
            code: Some(Bytes::from_static(&[0x00])),
            state: Some([(slot, B256::from(U256::from(333)))].into_iter().collect()),
            ..Default::default()
        };
        let overrides = FORECAST.state_overrides(&spec, &mut db, user(full.clone())).unwrap();
        assert_eq!(overrides.state.unwrap()[&Predeploys::BASE_TIME], full);

        // Invalid user input stays invalid.
        let invalid = AccountOverride { state_diff: Some(Default::default()), ..full };
        let overrides = FORECAST.state_overrides(&spec, &mut db, user(invalid)).unwrap();
        let mut state = State::builder().with_database(&mut db).build();
        assert!(apply_state_overrides(overrides.state.unwrap(), &mut state).is_err());
    }
}
