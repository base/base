use alloy_rpc_types::erc4337;
use serde::{Deserialize, Serialize};

// Re-export SendUserOperationResponse
pub use alloy_rpc_types::erc4337::SendUserOperationResponse;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum UserOperationRequest {
    EntryPointV06(erc4337::UserOperation),
    EntryPointV07(erc4337::PackedUserOperation),
}

// Tests
#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use alloy_primitives::{Address, Bytes, Uint};
    #[test]
    fn should_throw_error_when_deserializing_invalid() {
        const TEST_INVALID_USER_OPERATION: &str = r#"
        {
        "type": "EntryPointV06",
        "sender": "0x1111111111111111111111111111111111111111",
        "nonce": "0x0",
        "callGasLimit": "0x5208"
        }
    "#;
        let user_operation: Result<UserOperationRequest, serde_json::Error> =
            serde_json::from_str::<UserOperationRequest>(TEST_INVALID_USER_OPERATION);
        assert!(user_operation.is_err());
    }

    #[test]
    fn should_deserialize_v06() {
        const TEST_USER_OPERATION: &str = r#"
        {
            "type": "EntryPointV06",
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
        let user_operation: Result<UserOperationRequest, serde_json::Error> =
            serde_json::from_str::<UserOperationRequest>(TEST_USER_OPERATION);
        if user_operation.is_err() {
            panic!("Error: {:?}", user_operation.err());
        }
        let user_operation = user_operation.unwrap();
        match user_operation {
            UserOperationRequest::EntryPointV06(user_operation) => {
                assert_eq!(
                    user_operation.sender,
                    Address::from_str("0x1111111111111111111111111111111111111111").unwrap()
                );
                assert_eq!(user_operation.nonce, Uint::from(0));
                assert_eq!(user_operation.init_code, Bytes::from_str("0x").unwrap());
                assert_eq!(user_operation.call_data, Bytes::from_str("0x").unwrap());
                assert_eq!(user_operation.call_gas_limit, Uint::from(0x5208));
                assert_eq!(user_operation.verification_gas_limit, Uint::from(0x100000));
                assert_eq!(user_operation.pre_verification_gas, Uint::from(0x10000));
                assert_eq!(user_operation.max_fee_per_gas, Uint::from(0x59682f10));
                assert_eq!(
                    user_operation.max_priority_fee_per_gas,
                    Uint::from(0x3b9aca00)
                );
                assert_eq!(
                    user_operation.paymaster_and_data,
                    Bytes::from_str("0x").unwrap()
                );
                assert_eq!(user_operation.signature, Bytes::from_str("0x01").unwrap());
            }
            _ => {
                panic!("Expected EntryPointV06, got {user_operation:?}");
            }
        }
    }

    #[test]
    fn should_deserialize_v07() {
        const TEST_PACKED_USER_OPERATION: &str = r#"
        {
        "type": "EntryPointV07",
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
        let user_operation: Result<UserOperationRequest, serde_json::Error> =
            serde_json::from_str::<UserOperationRequest>(TEST_PACKED_USER_OPERATION);
        if user_operation.is_err() {
            panic!("Error: {:?}", user_operation.err());
        }
        let user_operation = user_operation.unwrap();
        match user_operation {
            UserOperationRequest::EntryPointV07(user_operation) => {
                assert_eq!(
                    user_operation.sender,
                    Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()
                );
                assert_eq!(user_operation.nonce, Uint::from(1));
                assert_eq!(
                    user_operation.call_data,
                    alloy_primitives::bytes!(
                        "0xb61d27f600000000000000000000000000000000000000000000000000000000000000c8"
                    )
                );
                assert_eq!(user_operation.call_gas_limit, Uint::from(0x2dc6c0));
                assert_eq!(user_operation.verification_gas_limit, Uint::from(0x1e8480));
                assert_eq!(user_operation.pre_verification_gas, Uint::from(0x186a0));
            }
            _ => {
                panic!("Expected EntryPointV07, got {user_operation:?}");
            }
        }
    }
}
