//! Core types for ERC-4337 user operations and validation.

use alloy_primitives::{Address, B256, ChainId, FixedBytes, U256};
use alloy_rpc_types::erc4337::{PackedUserOperation, UserOperation};
use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::entrypoints::{EntryPointVersion, hash_user_operation_v06, hash_user_operation_v07};

/// A user operation that can be either v0.6 or v0.7 format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum VersionedUserOperation {
    /// ERC-4337 v0.6 user operation.
    UserOperation(UserOperation),
    /// ERC-4337 v0.7 packed user operation.
    PackedUserOperation(PackedUserOperation),
}

impl VersionedUserOperation {
    /// Returns the max fee per gas for this user operation.
    pub const fn max_fee_per_gas(&self) -> U256 {
        match self {
            Self::UserOperation(op) => op.max_fee_per_gas,
            Self::PackedUserOperation(op) => op.max_fee_per_gas,
        }
    }

    /// Returns the max priority fee per gas for this user operation.
    pub const fn max_priority_fee_per_gas(&self) -> U256 {
        match self {
            Self::UserOperation(op) => op.max_priority_fee_per_gas,
            Self::PackedUserOperation(op) => op.max_priority_fee_per_gas,
        }
    }

    /// Returns the nonce of this user operation.
    pub const fn nonce(&self) -> U256 {
        match self {
            Self::UserOperation(op) => op.nonce,
            Self::PackedUserOperation(op) => op.nonce,
        }
    }

    /// Returns the sender address of this user operation.
    pub const fn sender(&self) -> Address {
        match self {
            Self::UserOperation(op) => op.sender,
            Self::PackedUserOperation(op) => op.sender,
        }
    }
}

/// A request to submit a user operation to the mempool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserOperationRequest {
    /// The user operation to submit.
    pub user_operation: VersionedUserOperation,
    /// The address of the `EntryPoint` contract.
    pub entry_point: Address,
    /// The chain ID for the operation.
    pub chain_id: ChainId,
}

impl UserOperationRequest {
    /// Computes the hash of this user operation request.
    pub fn hash(&self) -> Result<B256> {
        let entry_point_version = EntryPointVersion::try_from(self.entry_point)
            .map_err(|_| anyhow::anyhow!("Unknown entry point version: {:#x}", self.entry_point))?;

        match (&self.user_operation, entry_point_version) {
            (VersionedUserOperation::UserOperation(op), EntryPointVersion::V06) => {
                Ok(hash_user_operation_v06(op, self.entry_point, self.chain_id))
            }
            (VersionedUserOperation::PackedUserOperation(op), EntryPointVersion::V07) => {
                Ok(hash_user_operation_v07(op, self.entry_point, self.chain_id))
            }
            _ => Err(anyhow::anyhow!("Mismatched operation type and entry point version")),
        }
    }
}

/// The result of validating a user operation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationRequestValidationResult {
    /// The timestamp at which the validation expires.
    pub expiration_timestamp: u64,
    /// The amount of gas used during validation.
    pub gas_used: U256,
}

/// The result of validating a user operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    /// Whether the user operation is valid.
    pub valid: bool,
    /// The reason for validation failure, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The timestamp until which the validation is valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<u64>,
    /// The timestamp after which the validation is valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_after: Option<u64>,
    /// Additional context about the validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ValidationContext>,
}

/// Context information gathered during validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationContext {
    /// Stake information for the sender.
    pub sender_info: EntityStakeInfo,
    /// Stake information for the factory, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_info: Option<EntityStakeInfo>,
    /// Stake information for the paymaster, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster_info: Option<EntityStakeInfo>,
    /// Information about the aggregator, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator_info: Option<AggregatorInfo>,
}

/// Stake information for an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityStakeInfo {
    /// The address of the entity.
    pub address: Address,
    /// The amount staked by the entity.
    pub stake: U256,
    /// The delay in seconds before the stake can be withdrawn.
    pub unstake_delay_sec: u64,
    /// The deposit amount held by the `EntryPoint`.
    pub deposit: U256,
    /// Whether the entity meets the staking requirements.
    pub is_staked: bool,
}

/// Information about a signature aggregator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregatorInfo {
    /// The address of the aggregator contract.
    pub aggregator: Address,
    /// Stake information for the aggregator.
    pub stake_info: EntityStakeInfo,
}

/// A 32-byte hash identifying a user operation.
pub type UserOpHash = FixedBytes<32>;

/// A user operation wrapped with its computed hash.
#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct WrappedUserOperation {
    /// The underlying user operation.
    pub operation: VersionedUserOperation,
    /// The hash of the user operation.
    pub hash: UserOpHash,
}

impl WrappedUserOperation {
    /// Returns true if this operation has a higher max fee than another.
    pub fn has_higher_max_fee(&self, other: &Self) -> bool {
        self.operation.max_fee_per_gas() > other.operation.max_fee_per_gas()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_primitives::{Address, Uint};

    use super::*;

    #[test]
    fn deser_untagged_user_operation_without_type_field() {
        let json = r#"
        {
            "sender": "0x1111111111111111111111111111111111111111",
            "nonce": "0x0",
            "initCode": "0x",
            "callData": "0x",
            "callGasLimit": "0x5208",
            "verificationGasLimit": "0x100000",
            "preVerificationGas": "0x10000",
            "maxFeePerGas": "0x59682f10",
            "maxPriorityFeePerGas": "0x3b9aca00",
            "paymasterAndData": "0x",
            "signature": "0x01"
        }
        "#;

        let parsed: VersionedUserOperation =
            serde_json::from_str(json).expect("should deserialize as v0.6");
        match parsed {
            VersionedUserOperation::UserOperation(op) => {
                assert_eq!(
                    op.sender,
                    Address::from_str("0x1111111111111111111111111111111111111111").unwrap()
                );
                assert_eq!(op.nonce, Uint::from(0));
            }
            other => panic!("expected UserOperation, got {other:?}"),
        }
    }

    #[test]
    fn deser_untagged_packed_user_operation_without_type_field() {
        let json = r#"
        {
            "sender": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "nonce": "0x1",
            "factory": "0x2222222222222222222222222222222222222222",
            "factoryData": "0xabcdef1234560000000000000000000000000000000000000000000000000000",
            "callData": "0xb61d27f600000000000000000000000000000000000000000000000000000000000000c8",
            "callGasLimit": "0x2dc6c0",
            "verificationGasLimit": "0x1e8480",
            "preVerificationGas": "0x186a0",
            "maxFeePerGas": "0x77359400",
            "maxPriorityFeePerGas": "0x3b9aca00",
            "paymaster": "0x3333333333333333333333333333333333333333",
            "paymasterVerificationGasLimit": "0x186a0",
            "paymasterPostOpGasLimit": "0x27100",
            "paymasterData": "0xfafb00000000000000000000000000000000000000000000000000000000000064",
            "signature": "0xa3c5f1b90014e68abbbdc42e4b77b9accc0b7e1c5d0b5bcde1a47ba8faba00ff55c9a7de12e98b731766e35f6c51ab25c9b58cc0e7c4a33f25e75c51c6ad3c3a"
        }
        "#;

        let parsed: VersionedUserOperation =
            serde_json::from_str(json).expect("should deserialize as v0.7 packed");
        match parsed {
            VersionedUserOperation::PackedUserOperation(op) => {
                assert_eq!(
                    op.sender,
                    Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()
                );
                assert_eq!(op.nonce, Uint::from(1));
            }
            other => panic!("expected PackedUserOperation, got {other:?}"),
        }
    }
}
