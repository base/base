use super::entrypoints::{v06, v07, version::EntryPointVersion};
use alloy_primitives::{Address, B256, ChainId, FixedBytes, U256};
use alloy_rpc_types::erc4337;
pub use alloy_rpc_types::erc4337::SendUserOperationResponse;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum VersionedUserOperation {
    UserOperation(erc4337::UserOperation),
    PackedUserOperation(erc4337::PackedUserOperation),
}

impl VersionedUserOperation {
    pub fn max_fee_per_gas(&self) -> U256 {
        match self {
            VersionedUserOperation::UserOperation(op) => op.max_fee_per_gas,
            VersionedUserOperation::PackedUserOperation(op) => op.max_fee_per_gas,
        }
    }

    pub fn max_priority_fee_per_gas(&self) -> U256 {
        match self {
            VersionedUserOperation::UserOperation(op) => op.max_priority_fee_per_gas,
            VersionedUserOperation::PackedUserOperation(op) => op.max_priority_fee_per_gas,
        }
    }
    pub fn nonce(&self) -> U256 {
        match self {
            VersionedUserOperation::UserOperation(op) => op.nonce,
            VersionedUserOperation::PackedUserOperation(op) => op.nonce,
        }
    }

    pub fn sender(&self) -> Address {
        match self {
            VersionedUserOperation::UserOperation(op) => op.sender,
            VersionedUserOperation::PackedUserOperation(op) => op.sender,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserOperationRequest {
    pub user_operation: VersionedUserOperation,
    pub entry_point: Address,
    pub chain_id: ChainId,
}

impl UserOperationRequest {
    pub fn hash(&self) -> Result<B256> {
        let entry_point_version = EntryPointVersion::try_from(self.entry_point)
            .map_err(|_| anyhow::anyhow!("Unknown entry point version: {:#x}", self.entry_point))?;

        match (&self.user_operation, entry_point_version) {
            (VersionedUserOperation::UserOperation(op), EntryPointVersion::V06) => Ok(
                v06::hash_user_operation(op, self.entry_point, self.chain_id),
            ),
            (VersionedUserOperation::PackedUserOperation(op), EntryPointVersion::V07) => Ok(
                v07::hash_user_operation(op, self.entry_point, self.chain_id),
            ),
            _ => Err(anyhow::anyhow!(
                "Mismatched operation type and entry point version"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationRequestValidationResult {
    pub expiration_timestamp: u64,
    pub gas_used: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ValidationContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationContext {
    pub sender_info: EntityStakeInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_info: Option<EntityStakeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paymaster_info: Option<EntityStakeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator_info: Option<AggregatorInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityStakeInfo {
    pub address: Address,
    pub stake: U256,
    pub unstake_delay_sec: u64,
    pub deposit: U256,
    pub is_staked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregatorInfo {
    pub aggregator: Address,
    pub stake_info: EntityStakeInfo,
}

pub type UserOpHash = FixedBytes<32>;

#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct WrappedUserOperation {
    pub operation: VersionedUserOperation,
    pub hash: UserOpHash,
}

impl WrappedUserOperation {
    pub fn has_higher_max_fee(&self, other: &WrappedUserOperation) -> bool {
        self.operation.max_fee_per_gas() > other.operation.max_fee_per_gas()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use alloy_primitives::{Address, Uint};

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
            other => panic!("expected UserOperation, got {:?}", other),
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
            other => panic!("expected PackedUserOperation, got {:?}", other),
        }
    }
}
