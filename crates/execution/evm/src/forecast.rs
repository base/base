//! Pending block forecasts for the Denim timestamp schedule.

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{
    BlockId,
    state::{EvmOverrides, StateOverride},
};
use base_common_chains::{BaseUpgrade, BaseUpgradeExt, Upgrades};
use base_common_consensus::Predeploys;
use base_common_evm2::{BaseEvmTypes, BaseSpecId, BaseTime};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecError};
use evm2::{
    BlockStateAccumulator, Evm, Precompiles,
    env::BlockEnv,
    evm::{Database, Db},
    registry::HandlerResult,
};
use reth_chainspec::ChainSpecProvider;
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, EvmEnvFor};
use reth_rpc_eth_api::{
    FromEthApiError,
    helpers::{LoadState, SpawnBlocking},
};
use reth_rpc_eth_types::{PendingBlockEnv, PendingBlockEnvOrigin};

use crate::{BaseNextBlockEnvAttributes, UpgradeActivations};

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
    ) -> HandlerResult<EvmOverrides> {
        if !chain_spec.is_denim_active_at_timestamp(self.timestamp) {
            return Ok(overrides);
        }
        let spec = BaseSpecId::new(BaseUpgrade::from_timestamp(chain_spec, self.timestamp));
        let mut evm = Evm::new(
            spec,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(),
            Db::new(db),
            Precompiles::base(spec.into()),
        );
        let mut transition = BlockStateAccumulator::new();
        BaseTime::ensure_predeploy(
            &UpgradeActivations(chain_spec),
            self.timestamp,
            &mut evm,
            &mut transition,
        )?;
        let packed = evm.state_mut().storage_slot_untracked(
            &Predeploys::BASE_TIME,
            &BaseTime::TIMESTAMP_MILLIS_PART_SLOT,
        )?;
        let packed = (packed & !U256::from(u16::MAX)) | U256::from(self.timestamp_millis_part);

        let mut synthetic = StateOverride::default();
        for (address, account) in transition.accounts() {
            let code = account.current.as_ref().and_then(|info| {
                info.code.as_ref().or_else(|| {
                    transition.code().find(|(hash, _)| **hash == info.code_hash).map(|(_, c)| c)
                })
            });
            synthetic.entry(address).or_default().code = code.map(|code| code.original_bytes());
        }
        for (key, slot) in transition.storage() {
            synthetic
                .entry(key.address())
                .or_default()
                .state_diff
                .get_or_insert_default()
                .insert(B256::from(key.key()), B256::from(slot.current));
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
    use alloy_primitives::{Address, Bytes};
    use alloy_rpc_types_eth::state::AccountOverride;
    use evm2::{
        AccountInfo,
        bytecode::Bytecode,
        evm::{CacheDB, InMemoryDB},
    };
    use reth_chainspec::{ForkCondition, Hardfork};
    use reth_primitives_traits::SealedHeader;
    use reth_rpc_eth_types::cache::db::apply_state_overrides;

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
        let proxy = AccountInfo {
            code: Some(Bytecode::new_raw(BaseTime::proxy_bytecode())),
            ..Default::default()
        };
        db.insert_account_info(&Predeploys::BASE_TIME, proxy);
        let admin = U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice());
        let link = implementation.map(|address| U256::from_be_slice(address.as_slice()));
        for (slot, value) in [
            (BaseTime::ADMIN_SLOT, admin),
            (SLOT, packed),
            (BaseTime::IMPLEMENTATION_SLOT, link.unwrap_or_default()),
        ] {
            db.insert_account_storage(&Predeploys::BASE_TIME, &slot, &value);
        }
        db
    }

    /// Returns `db` with `overrides` applied on top.
    fn apply(db: &InMemoryDB, overrides: EvmOverrides) -> CacheDB<InMemoryDB> {
        let mut state = CacheDB::new(db.clone());
        apply_state_overrides(overrides.state.unwrap(), &mut state).unwrap();
        state
    }

    fn storage(state: &mut impl Database, slot: U256) -> U256 {
        state.get_storage(&Predeploys::BASE_TIME, &slot).unwrap()
    }

    fn millis_part(state: &mut impl Database) -> U256 {
        storage(state, SLOT) & U256::from(u16::MAX)
    }

    fn link(state: &mut impl Database) -> U256 {
        storage(state, BaseTime::IMPLEMENTATION_SLOT)
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
        assert_eq!(storage(&mut db, SLOT), upper | U256::from(800));
        let mut state = apply(&db, overrides);
        assert_eq!(storage(&mut state, SLOT), upper | U256::from(400));
        assert_eq!(link(&mut state), U256::from_be_slice(implementation.as_slice()));
        assert!(state.get_account(&BaseTime::IMPLEMENTATION_ADDRESS).unwrap().is_none());

        // An unlinked proxy is linked to the canonical implementation.
        let mut db = base_time_db(None, U256::ZERO);
        let overrides =
            FORECAST.state_overrides(&denim_spec(0), &mut db, EvmOverrides::default()).unwrap();
        let mut state = apply(&db, overrides);
        assert_eq!(millis_part(&mut state), U256::from(400));
        assert_eq!(
            link(&mut state),
            U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice())
        );
        assert_eq!(
            state.get_account(&BaseTime::IMPLEMENTATION_ADDRESS).unwrap().unwrap().code_hash,
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
        let mut state = apply(&db, overrides);
        assert_eq!(millis_part(&mut state), U256::from(300));
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
        let mut state = CacheDB::new(db);
        assert!(apply_state_overrides(overrides.state.unwrap(), &mut state).is_err());
    }
}
