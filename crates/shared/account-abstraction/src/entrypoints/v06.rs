/*
 * ERC-4337 v0.6 UserOperation Hash Calculation
 *
 * 1. Hash variable-length fields: initCode, callData, paymasterAndData
 * 2. Pack all fields into struct (using hashes from step 1, gas values as uint256)
 * 3. encodedHash = keccak256(abi.encode(packed struct))
 * 4. final hash = keccak256(abi.encode(encodedHash, entryPoint, chainId))
 *
 * Reference: rundler/crates/types/src/user_operation/v0_6.rs:927-934
 */
use alloy_primitives::{ChainId, U256};
use alloy_rpc_types::erc4337;
use alloy_sol_types::{SolValue, sol};
sol! {
    #[allow(missing_docs)]
    #[derive(Default, Debug, PartialEq, Eq)]
    struct UserOperationHashEncoded {
        bytes32 encodedHash;
        address entryPoint;
        uint256 chainId;
    }

    #[allow(missing_docs)]
    #[derive(Default, Debug, PartialEq, Eq)]
    struct UserOperationPackedForHash {
        address sender;
        uint256 nonce;
        bytes32 hashInitCode;
        bytes32 hashCallData;
        uint256 callGasLimit;
        uint256 verificationGasLimit;
        uint256 preVerificationGas;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        bytes32 hashPaymasterAndData;
    }
}

impl From<erc4337::UserOperation> for UserOperationPackedForHash {
    fn from(op: erc4337::UserOperation) -> Self {
        Self {
            sender: op.sender,
            nonce: op.nonce,
            hashInitCode: alloy_primitives::keccak256(op.init_code),
            hashCallData: alloy_primitives::keccak256(op.call_data),
            callGasLimit: U256::from(op.call_gas_limit),
            verificationGasLimit: U256::from(op.verification_gas_limit),
            preVerificationGas: U256::from(op.pre_verification_gas),
            maxFeePerGas: U256::from(op.max_fee_per_gas),
            maxPriorityFeePerGas: U256::from(op.max_priority_fee_per_gas),
            hashPaymasterAndData: alloy_primitives::keccak256(op.paymaster_and_data),
        }
    }
}

/// Computes the hash of a v0.6 user operation as defined by ERC-4337.
///
/// The hash is computed by packing the operation fields, hashing the packed data,
/// and then encoding with the entry point address and chain ID.
pub fn hash_user_operation(
    user_operation: &erc4337::UserOperation,
    entry_point: alloy_primitives::Address,
    chain_id: ChainId,
) -> alloy_primitives::B256 {
    let packed = UserOperationPackedForHash::from(user_operation.clone());
    let encoded = UserOperationHashEncoded {
        encodedHash: alloy_primitives::keccak256(packed.abi_encode()),
        entryPoint: entry_point,
        chainId: U256::from(chain_id),
    };
    alloy_primitives::keccak256(encoded.abi_encode())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256, address, b256, bytes};
    use alloy_rpc_types::erc4337;

    use super::*;

    #[test]
    fn test_hash_zeroed() {
        let entry_point_address_v0_6 = address!("66a15edcc3b50a663e72f1457ffd49b9ae284ddc");
        let chain_id = 1337;
        let user_op_with_zeroed_init_code = erc4337::UserOperation {
            sender: address!("0x0000000000000000000000000000000000000000"),
            nonce: U256::ZERO,
            init_code: Bytes::default(),
            call_data: Bytes::default(),
            call_gas_limit: U256::from(0),
            verification_gas_limit: U256::from(0),
            pre_verification_gas: U256::from(0),
            max_fee_per_gas: U256::from(0),
            max_priority_fee_per_gas: U256::from(0),
            paymaster_and_data: Bytes::default(),
            signature: Bytes::default(),
        };

        let hash =
            hash_user_operation(&user_op_with_zeroed_init_code, entry_point_address_v0_6, chain_id);
        assert_eq!(hash, b256!("dca97c3b49558ab360659f6ead939773be8bf26631e61bb17045bb70dc983b2d"));
    }

    #[test]
    fn test_hash_non_zeroed() {
        let entry_point_address_v0_6 = address!("66a15edcc3b50a663e72f1457ffd49b9ae284ddc");
        let chain_id = 1337;
        let user_op_with_non_zeroed_init_code = erc4337::UserOperation {
            sender: address!("0x1306b01bc3e4ad202612d3843387e94737673f53"),
            nonce: U256::from(8942),
            init_code: "0x6942069420694206942069420694206942069420".parse().unwrap(),
            call_data: "0x0000000000000000000000000000000000000000080085".parse().unwrap(),
            call_gas_limit: U256::from(10_000),
            verification_gas_limit: U256::from(100_000),
            pre_verification_gas: U256::from(100),
            max_fee_per_gas: U256::from(99_999),
            max_priority_fee_per_gas: U256::from(9_999_999),
            paymaster_and_data: bytes!(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            signature: bytes!("da0929f527cded8d0a1eaf2e8861d7f7e2d8160b7b13942f99dd367df4473a"),
        };

        let hash = hash_user_operation(
            &user_op_with_non_zeroed_init_code,
            entry_point_address_v0_6,
            chain_id,
        );
        assert_eq!(hash, b256!("484add9e4d8c3172d11b5feb6a3cc712280e176d278027cfa02ee396eb28afa1"));
    }
}
