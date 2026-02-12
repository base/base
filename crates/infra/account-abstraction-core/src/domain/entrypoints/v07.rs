/*
 * ERC-4337 v0.7 UserOperation Hash Calculation
 *
 * 1. Hash variable-length fields: initCode, callData, paymasterAndData
 * 2. Pack all fields into struct (using hashes from step 1, gas values as bytes32)
 * 3. encodedHash = keccak256(abi.encode(packed struct))
 * 4. final hash = keccak256(abi.encode(encodedHash, entryPoint, chainId))
 *
 * Reference: rundler/crates/types/src/user_operation/v0_7.rs:1094-1123
 */
use alloy_primitives::{Address, Bytes, ChainId, FixedBytes, U256, keccak256};
use alloy_rpc_types::erc4337;
use alloy_sol_types::{SolValue, sol};

sol!(
    #[allow(missing_docs)]
    #[derive(Default, Debug, PartialEq, Eq)]
    struct PackedUserOperation {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        bytes32 accountGasLimits;
        uint256 preVerificationGas;
        bytes32 gasFees;
        bytes paymasterAndData;
        bytes signature;
    }

    #[derive(Default, Debug, PartialEq, Eq)]
    struct UserOperationHashEncoded {
        bytes32 encodedHash;
        address entryPoint;
        uint256 chainId;
    }

    #[derive(Default, Debug, PartialEq, Eq)]
    struct UserOperationPackedForHash {
        address sender;
        uint256 nonce;
        bytes32 hashInitCode;
        bytes32 hashCallData;
        bytes32 accountGasLimits;
        uint256 preVerificationGas;
        bytes32 gasFees;
        bytes32 hashPaymasterAndData;
    }
);

impl From<erc4337::PackedUserOperation> for PackedUserOperation {
    fn from(uo: erc4337::PackedUserOperation) -> Self {
        let init_code = if let Some(factory) = uo.factory {
            let mut init_code = factory.to_vec();
            init_code.extend_from_slice(&uo.factory_data.clone().unwrap_or_default());
            Bytes::from(init_code)
        } else {
            Bytes::new()
        };
        let account_gas_limits =
            pack_u256_pair_to_bytes32(uo.verification_gas_limit, uo.call_gas_limit);
        let gas_fees = pack_u256_pair_to_bytes32(uo.max_priority_fee_per_gas, uo.max_fee_per_gas);
        let pvgl: [u8; 16] =
            uo.paymaster_verification_gas_limit.unwrap_or_default().to::<u128>().to_be_bytes();
        let pogl: [u8; 16] =
            uo.paymaster_post_op_gas_limit.unwrap_or_default().to::<u128>().to_be_bytes();
        let paymaster_and_data = if let Some(paymaster) = uo.paymaster {
            let mut paymaster_and_data = paymaster.to_vec();
            paymaster_and_data.extend_from_slice(&pvgl);
            paymaster_and_data.extend_from_slice(&pogl);
            paymaster_and_data.extend_from_slice(&uo.paymaster_data.unwrap());
            Bytes::from(paymaster_and_data)
        } else {
            Bytes::new()
        };
        PackedUserOperation {
            sender: uo.sender,
            nonce: uo.nonce,
            initCode: init_code,
            callData: uo.call_data.clone(),
            accountGasLimits: account_gas_limits,
            preVerificationGas: U256::from(uo.pre_verification_gas),
            gasFees: gas_fees,
            paymasterAndData: paymaster_and_data,
            signature: uo.signature.clone(),
        }
    }
}
fn pack_u256_pair_to_bytes32(high: U256, low: U256) -> FixedBytes<32> {
    let mask = (U256::from(1u64) << 128) - U256::from(1u64);
    let hi = high & mask;
    let lo = low & mask;
    let combined: U256 = (hi << 128) | lo;
    FixedBytes::from(combined.to_be_bytes::<32>())
}

fn hash_packed_user_operation(
    puo: &PackedUserOperation,
    entry_point: Address,
    chain_id: u64,
) -> FixedBytes<32> {
    let hash_init_code = alloy_primitives::keccak256(&puo.initCode);
    let hash_call_data = alloy_primitives::keccak256(&puo.callData);
    let hash_paymaster_and_data = alloy_primitives::keccak256(&puo.paymasterAndData);

    let packed_for_hash = UserOperationPackedForHash {
        sender: puo.sender,
        nonce: puo.nonce,
        hashInitCode: hash_init_code,
        hashCallData: hash_call_data,
        accountGasLimits: puo.accountGasLimits,
        preVerificationGas: puo.preVerificationGas,
        gasFees: puo.gasFees,
        hashPaymasterAndData: hash_paymaster_and_data,
    };

    let hashed = alloy_primitives::keccak256(packed_for_hash.abi_encode());

    let encoded = UserOperationHashEncoded {
        encodedHash: hashed,
        entryPoint: entry_point,
        chainId: U256::from(chain_id),
    };

    keccak256(encoded.abi_encode())
}

pub fn hash_user_operation(
    user_operation: &erc4337::PackedUserOperation,
    entry_point: Address,
    chain_id: ChainId,
) -> FixedBytes<32> {
    let packed = PackedUserOperation::from(user_operation.clone());
    hash_packed_user_operation(&packed, entry_point, chain_id)
}

#[cfg(test)]
mod test {
    use alloy_primitives::{Bytes, U256, address, b256, bytes, uint};

    use super::*;

    #[test]
    fn test_hash() {
        let puo = PackedUserOperation {
            sender: address!("b292Cf4a8E1fF21Ac27C4f94071Cd02C022C414b"),
            nonce: uint!(0xF83D07238A7C8814A48535035602123AD6DBFA63000000000000000000000001_U256),
            initCode: Bytes::default(),  // Empty
            callData:
    bytes!("e9ae5c53000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000
    0000000000000000000000000000000000001d8b292cf4a8e1ff21ac27c4f94071cd02c022c414b00000000000000000000000000000000000000000000000000000000000000009517e29f000000000000000000
    0000000000000000000000000000000000000000000002000000000000000000000000ad6330089d9a1fe89f4020292e1afe9969a5a2fc00000000000000000000000000000000000000000000000000000000000
    0006000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000000000000000000015180000000000000000000000000000000000000
    00000000000000000000000000000000000000000000000000000000000000000000000000000000018e2fbe898000000000000000000000000000000000000000000000000000000000000000800000000000000
    0000000000000000000000000000000000000000000000000800000000000000000000000002372912728f93ab3daaaebea4f87e6e28476d987000000000000000000000000000000000000000000000000002386
    f26fc10000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000"),
            accountGasLimits: b256!("000000000000000000000000000114fc0000000000000000000000000012c9b5"),
            preVerificationGas: U256::from(48916),
            gasFees: b256!("000000000000000000000000524121000000000000000000000000109a4a441a"),
            paymasterAndData: Bytes::default(),  // Empty
            signature: bytes!("3c7bfe22c9c2ef8994a9637bcc4df1741c5dc0c25b209545a7aeb20f7770f351479b683bd17c4d55bc32e2a649c8d2dff49dcfcc1f3fd837bcd88d1e69a434cf1c"),
        };

        let expected_hash =
            b256!("e486401370d145766c3cf7ba089553214a1230d38662ae532c9b62eb6dadcf7e");
        let uo = hash_packed_user_operation(
            &puo,
            address!("0x0000000071727De22E5E9d8BAf0edAc6f37da032"),
            11155111,
        );

        assert_eq!(uo, expected_hash);
    }
}
