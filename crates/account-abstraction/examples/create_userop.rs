//! Example tool to create and sign a UserOperation
//! 
//! Usage:
//!   cargo run --example create_userop -- \
//!     --private-key <private_key> \
//!     --sender <wallet_address> \
//!     --entry-point <entry_point_address> \
//!     --chain-id <chain_id> \
//!     [--nonce <nonce>] \
//!     [--call-data <call_data>]

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::SolValue;
use base_reth_account_abstraction::{UserOperation, UserOperationV06};
use clap::Parser;
use eyre::Result;
use alloy_primitives::hex;

#[derive(Parser, Debug)]
#[command(name = "create_userop")]
#[command(about = "Create and sign a UserOperation for EIP-4337")]
struct Args {
    /// Private key of the wallet owner (without 0x prefix)
    #[arg(long)]
    private_key: String,

    /// Address of the smart contract wallet
    #[arg(long)]
    sender: Address,

    /// EntryPoint contract address
    #[arg(long)]
    entry_point: Address,

    /// Chain ID
    #[arg(long)]
    chain_id: u64,

    /// Nonce (default: 0)
    #[arg(long, default_value = "0")]
    nonce: u64,

    /// Call data (default: 0x - no operation)
    #[arg(long, default_value = "0x")]
    call_data: String,

    /// Init code (for deploying the wallet)
    #[arg(long, default_value = "0x")]
    init_code: String,

    /// Call gas limit (default: 100000)
    #[arg(long, default_value = "100000")]
    call_gas_limit: u64,

    /// Verification gas limit (default: 300000)
    #[arg(long, default_value = "300000")]
    verification_gas_limit: u64,

    /// Pre-verification gas (default: 50000)
    #[arg(long, default_value = "50000")]
    pre_verification_gas: u64,

    /// Max fee per gas in gwei (default: 2 gwei)
    #[arg(long, default_value = "2")]
    max_fee_per_gas_gwei: u64,

    /// Max priority fee per gas in gwei (default: 1 gwei)
    #[arg(long, default_value = "1")]
    max_priority_fee_per_gas_gwei: u64,
}

/// Solidity types for hash encoding (v0.6)
mod hash_types {
    use alloy_sol_types::sol;

    sol! {
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

        struct UserOperationHashEncoded {
            bytes32 encodedHash;
            address entryPoint;
            uint256 chainId;
        }
    }
}

/// Calculate the user operation hash for v0.6
fn calculate_user_op_hash_v06(
    user_op: &UserOperationV06,
    entry_point: Address,
    chain_id: u64,
) -> B256 {
    use hash_types::*;

    // Hash the dynamic fields
    let hash_init_code = keccak256(&user_op.init_code);
    let hash_call_data = keccak256(&user_op.call_data);
    let hash_paymaster_and_data = keccak256(&user_op.paymaster_and_data);

    // Create the packed structure
    let packed = UserOperationPackedForHash {
        sender: user_op.sender,
        nonce: user_op.nonce,
        hashInitCode: hash_init_code,
        hashCallData: hash_call_data,
        callGasLimit: user_op.call_gas_limit,
        verificationGasLimit: user_op.verification_gas_limit,
        preVerificationGas: user_op.pre_verification_gas,
        maxFeePerGas: user_op.max_fee_per_gas,
        maxPriorityFeePerGas: user_op.max_priority_fee_per_gas,
        hashPaymasterAndData: hash_paymaster_and_data,
    };

    // Hash the packed structure
    let hashed = keccak256(packed.abi_encode());

    // Create the final encoded structure
    let encoded = UserOperationHashEncoded {
        encodedHash: hashed,
        entryPoint: entry_point,
        chainId: U256::from(chain_id),
    };

    // Final hash
    keccak256(encoded.abi_encode())
}

/// Sign a hash using a private key
fn sign_hash(hash: B256, private_key: &str) -> Result<Bytes> {
    use k256::ecdsa::SigningKey;
    
    // Parse private key
    let key_bytes = hex::decode(private_key.trim_start_matches("0x"))?;
    
    // Ensure we have exactly 32 bytes
    if key_bytes.len() != 32 {
        return Err(eyre::eyre!("Invalid private key length: expected 32 bytes, got {}", key_bytes.len()));
    }
    
    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&key_bytes);
    
    let signing_key = SigningKey::from_bytes(&key_array.into())?;
    
    // Sign the hash using ECDSA
    let (sig, recid) = signing_key.sign_prehash_recoverable(&hash[..])?;
    
    // Convert to Ethereum signature format (r, s, v)
    let r_bytes: [u8; 32] = sig.r().to_bytes().into();
    let s_bytes: [u8; 32] = sig.s().to_bytes().into();
    let v = recid.to_byte() + 27;
    
    // Combine into signature bytes
    let mut signature = Vec::with_capacity(65);
    signature.extend_from_slice(&r_bytes);
    signature.extend_from_slice(&s_bytes);
    signature.push(v);
    
    Ok(Bytes::from(signature))
}

fn main() -> Result<()> {
    let args = Args::parse();

    println!("🔧 Creating UserOperation (v0.6)...\n");

    // Parse call data and init code
    let call_data = if args.call_data.starts_with("0x") {
        Bytes::from(hex::decode(&args.call_data[2..])?)
    } else {
        Bytes::from(hex::decode(&args.call_data)?)
    };

    let init_code = if args.init_code.starts_with("0x") {
        Bytes::from(hex::decode(&args.init_code[2..])?)
    } else {
        Bytes::from(hex::decode(&args.init_code)?)
    };

    // Create unsigned user operation
    let mut user_op = UserOperationV06 {
        sender: args.sender,
        nonce: U256::from(args.nonce),
        init_code,
        call_data,
        call_gas_limit: U256::from(args.call_gas_limit),
        verification_gas_limit: U256::from(args.verification_gas_limit),
        pre_verification_gas: U256::from(args.pre_verification_gas),
        max_fee_per_gas: U256::from(args.max_fee_per_gas_gwei) * U256::from(1_000_000_000u64),
        max_priority_fee_per_gas: U256::from(args.max_priority_fee_per_gas_gwei) * U256::from(1_000_000_000u64),
        paymaster_and_data: Bytes::new(),
        signature: Bytes::new(), // Will be filled after hashing
    };

    // Calculate user operation hash
    let user_op_hash = calculate_user_op_hash_v06(&user_op, args.entry_point, args.chain_id);
    println!("📝 UserOperation Hash: 0x{}", hex::encode(user_op_hash));

    // Sign the hash
    let signature = sign_hash(user_op_hash, &args.private_key)?;
    user_op.signature = signature;

    println!("✅ Signature: 0x{}\n", hex::encode(&user_op.signature));

    // Print the complete UserOperation as JSON
    let user_op_wrapper = UserOperation::V06(user_op);
    let json = serde_json::to_string_pretty(&user_op_wrapper)?;
    
    println!("📦 Complete UserOperation:");
    println!("{}\n", json);

    println!("🚀 You can now send this UserOperation using:");
    println!("curl -X POST http://localhost:8545 \\");
    println!("  -H 'Content-Type: application/json' \\");
    println!("  -d '{{");
    println!("    \"jsonrpc\": \"2.0\",");
    println!("    \"id\": 1,");
    println!("    \"method\": \"eth_sendUserOperation\",");
    println!("    \"params\": [");
    println!("      {},", json.replace('\n', "\n      "));
    println!("      \"{}\"", args.entry_point);
    println!("    ]");
    println!("  }}'");

    Ok(())
}

