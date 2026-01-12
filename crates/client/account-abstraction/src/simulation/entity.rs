//! Entity stake and deposit information provider
//!
//! This module provides functionality to query stake and deposit information
//! for entities (accounts, factories, paymasters, aggregators) from the EntryPoint.

use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use std::sync::Arc;

use crate::contracts::{ENTRYPOINT_V06_ADDRESS, ENTRYPOINT_V07_ADDRESS, ENTRYPOINT_V08_ADDRESS};
use crate::entrypoint::EntryPointVersion;
use crate::estimation::SimulationProvider;
use crate::rpc::UserOperation;

use super::types::{
    AggregatorStakeInfo, Entities, EntitiesInfo, StakeInfo, StakingConfig, ValidationError,
};

// Define the DepositInfo struct and getDepositInfo interface
// This is consistent across all EntryPoint versions
sol! {
    /// Deposit info for an address
    /// Returned by EntryPoint.getDepositInfo(address)
    #[derive(Debug, Default)]
    struct DepositInfo {
        uint256 deposit;      // Amount deposited (for gas payment)
        bool staked;          // True if entity has stake
        uint112 stake;        // Amount staked
        uint32 unstakeDelaySec;  // Unstake delay in seconds
        uint48 withdrawTime;  // Time when stake can be withdrawn (0 if not withdrawing)
    }

    /// Stake info (simpler version returned by simulateValidation)
    #[derive(Debug, Default)]
    struct StakeInfoSol {
        uint256 stake;
        uint256 unstakeDelaySec;
    }

    /// Interface for querying deposit info from EntryPoint
    interface IStakeManager {
        /// Get deposit info for an account
        function getDepositInfo(address account) external view returns (DepositInfo memory info);

        /// Get the balance (deposit) for an account
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Provider for entity stake and deposit information
pub struct EntityInfoProvider<P> {
    provider: Arc<P>,
    staking_config: StakingConfig,
}

impl<P> EntityInfoProvider<P>
where
    P: SimulationProvider,
{
    /// Create a new entity info provider
    pub fn new(provider: Arc<P>) -> Self {
        Self {
            provider,
            staking_config: StakingConfig::default(),
        }
    }

    /// Create with custom staking configuration
    pub fn with_staking_config(provider: Arc<P>, staking_config: StakingConfig) -> Self {
        Self {
            provider,
            staking_config,
        }
    }

    /// Get the staking configuration
    pub fn staking_config(&self) -> &StakingConfig {
        &self.staking_config
    }

    /// Get deposit info for an address from the EntryPoint
    pub async fn get_deposit_info(
        &self,
        address: Address,
        entry_point: Address,
    ) -> Result<StakeInfo, ValidationError> {
        // Encode the getDepositInfo call
        let call = IStakeManager::getDepositInfoCall { account: address };
        let calldata = Bytes::from(call.abi_encode());

        // Execute eth_call
        let result = self
            .provider
            .eth_call(entry_point, calldata, BlockId::pending(), None)
            .await
            .map_err(|e| ValidationError::ProviderError(e))?;

        // Check if we got a successful result
        let result_bytes = match result {
            crate::estimation::SimulationCallResult::Success(data) => data,
            crate::estimation::SimulationCallResult::Revert(data) => {
                return Err(ValidationError::SimulationFailed(format!(
                    "getDepositInfo reverted: 0x{}",
                    hex::encode(&data)
                )));
            }
        };

        // Decode the result
        let deposit_info = IStakeManager::getDepositInfoCall::abi_decode_returns(&result_bytes)
            .map_err(|e| ValidationError::DecodingError(format!("Failed to decode DepositInfo: {}", e)))?;

        // Convert to our StakeInfo type
        let is_staked = self.is_entity_staked(&deposit_info);

        Ok(StakeInfo {
            address,
            stake: U256::from(deposit_info.stake),
            unstake_delay_sec: deposit_info.unstakeDelaySec as u64,
            deposit: deposit_info.deposit,
            is_staked,
        })
    }

    /// Check if an entity is considered "staked" based on config
    fn is_entity_staked(&self, deposit_info: &DepositInfo) -> bool {
        let stake = U256::from(deposit_info.stake);
        let unstake_delay = deposit_info.unstakeDelaySec as u64;

        stake >= self.staking_config.min_stake_value
            && unstake_delay >= self.staking_config.min_unstake_delay
    }

    /// Get stake info for all entities involved in a UserOperation
    pub async fn get_entities_info(
        &self,
        entities: &Entities,
        entry_point: Address,
    ) -> Result<EntitiesInfo, ValidationError> {
        // Fetch all entity info in parallel
        let sender_future = self.get_deposit_info(entities.sender, entry_point);

        let factory_future = async {
            if let Some(factory) = entities.factory {
                Some(self.get_deposit_info(factory, entry_point).await)
            } else {
                None
            }
        };

        let paymaster_future = async {
            if let Some(paymaster) = entities.paymaster {
                Some(self.get_deposit_info(paymaster, entry_point).await)
            } else {
                None
            }
        };

        let aggregator_future = async {
            if let Some(aggregator) = entities.aggregator {
                Some(self.get_deposit_info(aggregator, entry_point).await)
            } else {
                None
            }
        };

        // Execute all futures
        let (sender_result, factory_result, paymaster_result, aggregator_result) =
            tokio::join!(sender_future, factory_future, paymaster_future, aggregator_future);

        let sender = sender_result?;

        let factory = match factory_result {
            Some(Ok(info)) => Some(info),
            Some(Err(e)) => return Err(e),
            None => None,
        };

        let paymaster = match paymaster_result {
            Some(Ok(info)) => Some(info),
            Some(Err(e)) => return Err(e),
            None => None,
        };

        let aggregator = match aggregator_result {
            Some(Ok(stake_info)) => Some(AggregatorStakeInfo {
                aggregator: stake_info.address,
                stake_info,
            }),
            Some(Err(e)) => return Err(e),
            None => None,
        };

        Ok(EntitiesInfo {
            sender,
            factory,
            paymaster,
            aggregator,
        })
    }
}

/// Entity extraction methods (no trait bounds needed - pure functions)
impl<P> EntityInfoProvider<P> {
    /// Extract entity addresses from a UserOperation
    pub fn extract_entities(user_op: &UserOperation, entry_point: Address) -> Entities {
        match user_op {
            UserOperation::V06(op) => Self::extract_entities_v06(op),
            UserOperation::V07(op) => Self::extract_entities_v07(op, entry_point),
        }
    }

    /// Extract entities from a v0.6 UserOperation
    pub fn extract_entities_v06(op: &crate::rpc::UserOperationV06) -> Entities {
        let sender = op.sender;

        // Factory is first 20 bytes of initCode (if present)
        let factory = if op.init_code.len() >= 20 {
            Some(Address::from_slice(&op.init_code[..20]))
        } else {
            None
        };

        // Paymaster is first 20 bytes of paymasterAndData (if present)
        let paymaster = if op.paymaster_and_data.len() >= 20 {
            Some(Address::from_slice(&op.paymaster_and_data[..20]))
        } else {
            None
        };

        Entities {
            sender,
            factory,
            paymaster,
            aggregator: None, // Aggregator is determined during validation
        }
    }

    /// Extract entities from a v0.7 UserOperation
    pub fn extract_entities_v07(op: &crate::rpc::UserOperationV07, _entry_point: Address) -> Entities {
        let sender = op.sender;

        // Factory is explicit in v0.7
        let factory = if !op.factory.is_zero() {
            Some(op.factory)
        } else {
            None
        };

        // Paymaster is explicit in v0.7
        let paymaster = if !op.paymaster.is_zero() {
            Some(op.paymaster)
        } else {
            None
        };

        Entities {
            sender,
            factory,
            paymaster,
            aggregator: None, // Aggregator is determined during validation
        }
    }
}

/// Check if an address is the EntryPoint
pub fn is_entry_point(address: Address) -> bool {
    address == ENTRYPOINT_V06_ADDRESS
        || address == ENTRYPOINT_V07_ADDRESS
        || address == ENTRYPOINT_V08_ADDRESS
}

/// Get the EntryPoint address for a version
pub fn entry_point_address(version: EntryPointVersion) -> Address {
    match version {
        EntryPointVersion::V06 => ENTRYPOINT_V06_ADDRESS,
        EntryPointVersion::V07 => ENTRYPOINT_V07_ADDRESS,
        EntryPointVersion::V08 => ENTRYPOINT_V08_ADDRESS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_entities_v06() {
        use crate::rpc::UserOperationV06;

        let sender = Address::repeat_byte(0x01);
        let factory = Address::repeat_byte(0x02);
        let paymaster = Address::repeat_byte(0x03);

        // Create init_code with factory address
        let mut init_code = factory.to_vec();
        init_code.extend_from_slice(&[0x12, 0x34]); // factory data

        // Create paymasterAndData with paymaster address
        let mut paymaster_and_data = paymaster.to_vec();
        paymaster_and_data.extend_from_slice(&[0x56, 0x78]); // paymaster data

        let op = UserOperationV06 {
            sender,
            nonce: U256::ZERO,
            init_code: Bytes::from(init_code),
            call_data: Bytes::default(),
            call_gas_limit: U256::ZERO,
            verification_gas_limit: U256::ZERO,
            pre_verification_gas: U256::ZERO,
            max_fee_per_gas: U256::ZERO,
            max_priority_fee_per_gas: U256::ZERO,
            paymaster_and_data: Bytes::from(paymaster_and_data),
            signature: Bytes::default(),
        };

        let entities = EntityInfoProvider::<()>::extract_entities_v06(&op);

        assert_eq!(entities.sender, sender);
        assert_eq!(entities.factory, Some(factory));
        assert_eq!(entities.paymaster, Some(paymaster));
        assert_eq!(entities.aggregator, None);
    }

    #[test]
    fn test_is_entry_point() {
        assert!(is_entry_point(ENTRYPOINT_V06_ADDRESS));
        assert!(is_entry_point(ENTRYPOINT_V07_ADDRESS));
        assert!(is_entry_point(ENTRYPOINT_V08_ADDRESS));
        assert!(!is_entry_point(Address::ZERO));
        assert!(!is_entry_point(Address::repeat_byte(0x01)));
    }
}

