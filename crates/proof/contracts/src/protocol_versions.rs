//! `ProtocolVersions` contract bindings and schedule reconstruction helpers.

use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, sol};
use base_proof_primitives::{
    ProtocolVersionsSchedule, ProtocolVersionsScheduleEntry, ProtocolVersionsScheduleError,
};

use crate::ContractError;

sol! {
    /// `ProtocolVersions` schedule interface.
    #[sol(rpc)]
    interface IProtocolVersions {
        struct Upgrade {
            string name;
            uint64 timestamp;
            uint256 protocolVersion;
            bytes32 scheduleId;
        }

        event UpgradeRegistered(bytes32 indexed key, uint256 indexed index, string upgradeId, uint256 protocolVersion);
        event TimestampSet(bytes32 indexed key, uint256 timestamp);

        function l2ChainId() external view returns (uint256);
        function scheduleId() external view returns (bytes32);
        function getSchedule() external view returns (Upgrade[] memory);
    }
}

/// Concrete `ProtocolVersions` client backed by an Alloy provider.
#[derive(Debug, Clone)]
pub struct ProtocolVersionsContractClient {
    provider: RootProvider,
}

impl ProtocolVersionsContractClient {
    /// Creates a new client connected to the given L1 RPC URL.
    pub fn new(l1_rpc_url: url::Url) -> Result<Self, ContractError> {
        Ok(Self { provider: RootProvider::new_http(l1_rpc_url) })
    }

    /// Creates a new client from an existing provider.
    pub const fn from_provider(provider: RootProvider) -> Self {
        Self { provider }
    }

    /// Returns the current full schedule for a `ProtocolVersions` contract.
    pub async fn current_schedule(
        &self,
        protocol_versions_address: Address,
    ) -> Result<ProtocolVersionsSchedule, ContractError> {
        let contract = self.contract(protocol_versions_address);
        let (l2_chain_id, schedule_id, schedule) = futures::try_join!(
            async { self.l2_chain_id(&contract).await },
            async {
                contract
                    .scheduleId()
                    .call()
                    .await
                    .map_err(|source| ContractError::call("ProtocolVersions.scheduleId failed", source))
            },
            async {
                contract
                    .getSchedule()
                    .call()
                    .await
                    .map_err(|source| ContractError::call("ProtocolVersions.getSchedule failed", source))
            },
        )?;

        let schedule = ProtocolVersionsSchedule {
            upgrades: schedule
                .into_iter()
                .map(|entry| ProtocolVersionsScheduleEntry {
                    name: entry.name,
                    timestamp: (entry.timestamp != 0).then_some(entry.timestamp),
                })
                .collect(),
        };

        self.validate_schedule_hash(protocol_versions_address, l2_chain_id, &schedule, schedule_id)?;
        Ok(schedule)
    }

    /// Resolves the historical schedule that produced `activation_schedule_hash`.
    pub async fn schedule_for_hash(
        &self,
        protocol_versions_address: Address,
        activation_schedule_hash: B256,
    ) -> Result<ProtocolVersionsSchedule, ContractError> {
        let contract = self.contract(protocol_versions_address);
        let l2_chain_id = self.l2_chain_id(&contract).await?;

        let current_schedule = self.current_schedule(protocol_versions_address).await?;
        let current_hash = ProtocolVersionsSchedule::compute_schedule_hash_parts(
            l2_chain_id,
            protocol_versions_address,
            &current_schedule.upgrades,
        )
        .map_err(Self::schedule_error)?;
        if current_hash == activation_schedule_hash {
            return Ok(current_schedule);
        }

        let empty_schedule = ProtocolVersionsSchedule::default();
        let empty_hash = ProtocolVersionsSchedule::compute_schedule_hash_parts(
            l2_chain_id,
            protocol_versions_address,
            &empty_schedule.upgrades,
        )
        .map_err(Self::schedule_error)?;
        if empty_hash == activation_schedule_hash {
            return Ok(empty_schedule);
        }

        let logs = self.schedule_logs(protocol_versions_address).await?;
        self.replay_schedule_logs(
            protocol_versions_address,
            l2_chain_id,
            activation_schedule_hash,
            logs,
        )
    }

    fn contract(
        &self,
        protocol_versions_address: Address,
    ) -> IProtocolVersions::IProtocolVersionsInstance<&RootProvider> {
        IProtocolVersions::IProtocolVersionsInstance::new(protocol_versions_address, &self.provider)
    }

    async fn l2_chain_id(
        &self,
        contract: &IProtocolVersions::IProtocolVersionsInstance<&RootProvider>,
    ) -> Result<u64, ContractError> {
        let chain_id: U256 = contract
            .l2ChainId()
            .call()
            .await
            .map_err(|source| ContractError::call("ProtocolVersions.l2ChainId failed", source))?;
        chain_id
            .try_into()
            .map_err(|_| ContractError::validation("ProtocolVersions.l2ChainId overflows u64"))
    }

    async fn schedule_logs(&self, protocol_versions_address: Address) -> Result<Vec<Log>, ContractError> {
        let upgrade_logs = self
            .provider
            .get_logs(
                &Filter::new()
                    .address(protocol_versions_address)
                    .event_signature(IProtocolVersions::UpgradeRegistered::SIGNATURE_HASH),
            )
            .await
            .map_err(|source| {
                ContractError::provider("ProtocolVersions UpgradeRegistered logs failed", source)
            })?;

        let timestamp_logs = self
            .provider
            .get_logs(
                &Filter::new()
                    .address(protocol_versions_address)
                    .event_signature(IProtocolVersions::TimestampSet::SIGNATURE_HASH),
            )
            .await
            .map_err(|source| {
                ContractError::provider("ProtocolVersions TimestampSet logs failed", source)
            })?;

        let mut logs = Vec::with_capacity(upgrade_logs.len() + timestamp_logs.len());
        logs.extend(upgrade_logs);
        logs.extend(timestamp_logs);
        logs.sort_unstable_by_key(|log| {
            (log.block_number.unwrap_or_default(), log.log_index.unwrap_or_default())
        });
        Ok(logs)
    }

    fn replay_schedule_logs(
        &self,
        protocol_versions_address: Address,
        l2_chain_id: u64,
        activation_schedule_hash: B256,
        logs: Vec<Log>,
    ) -> Result<ProtocolVersionsSchedule, ContractError> {
        let mut schedule = ProtocolVersionsSchedule::default();
        let mut indices = HashMap::<String, usize>::new();

        for log in logs {
            let Some(topic0) = log.topic0().copied() else {
                continue;
            };

            if topic0 == IProtocolVersions::UpgradeRegistered::SIGNATURE_HASH {
                let decoded = log.log_decode::<IProtocolVersions::UpgradeRegistered>().map_err(|error| {
                    ContractError::validation(format!(
                        "failed to decode ProtocolVersions UpgradeRegistered log: {error}"
                    ))
                })?;
                let entry = decoded.inner.data;
                let name = ProtocolVersionsScheduleEntry::name_from_key(entry.key)
                    .map_err(Self::schedule_error)?;
                let index: usize = entry.index.try_into().map_err(|_| {
                    ContractError::validation("ProtocolVersions upgrade index overflows usize")
                })?;
                if index != schedule.upgrades.len() {
                    return Err(ContractError::validation(format!(
                        "ProtocolVersions log order mismatch: expected index {}, got {index}",
                        schedule.upgrades.len()
                    )));
                }
                indices.insert(name.clone(), index);
                schedule.upgrades.push(ProtocolVersionsScheduleEntry { name, timestamp: None });
            } else if topic0 == IProtocolVersions::TimestampSet::SIGNATURE_HASH {
                let decoded = log.log_decode::<IProtocolVersions::TimestampSet>().map_err(|error| {
                    ContractError::validation(format!(
                        "failed to decode ProtocolVersions TimestampSet log: {error}"
                    ))
                })?;
                let entry = decoded.inner.data;
                let name = ProtocolVersionsScheduleEntry::name_from_key(entry.key)
                    .map_err(Self::schedule_error)?;
                let Some(index) = indices.get(&name).copied() else {
                    return Err(ContractError::validation(format!(
                        "ProtocolVersions timestamp log referenced unknown upgrade {name}"
                    )));
                };
                let timestamp: u64 = entry.timestamp.try_into().map_err(|_| {
                    ContractError::validation("ProtocolVersions timestamp overflows u64")
                })?;
                schedule.upgrades[index].timestamp = (timestamp != 0).then_some(timestamp);
            }

            let computed_hash = ProtocolVersionsSchedule::compute_schedule_hash_parts(
                l2_chain_id,
                protocol_versions_address,
                &schedule.upgrades,
            )
            .map_err(Self::schedule_error)?;
            if computed_hash == activation_schedule_hash {
                return Ok(schedule);
            }
        }

        Err(ContractError::validation(format!(
            "failed to reconstruct ProtocolVersions schedule for hash {activation_schedule_hash}"
        )))
    }

    fn validate_schedule_hash(
        &self,
        protocol_versions_address: Address,
        l2_chain_id: u64,
        schedule: &ProtocolVersionsSchedule,
        expected_hash: B256,
    ) -> Result<(), ContractError> {
        let computed_hash = ProtocolVersionsSchedule::compute_schedule_hash_parts(
            l2_chain_id,
            protocol_versions_address,
            &schedule.upgrades,
        )
        .map_err(Self::schedule_error)?;
        if computed_hash != expected_hash {
            return Err(ContractError::validation(format!(
                "ProtocolVersions schedule hash mismatch: expected {expected_hash}, got {computed_hash}"
            )));
        }
        Ok(())
    }

    fn schedule_error(error: ProtocolVersionsScheduleError) -> ContractError {
        ContractError::validation(error.to_string())
    }
}
