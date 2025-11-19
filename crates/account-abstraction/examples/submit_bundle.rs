//! Tool to create a bundle and submit directly to EntryPoint
//! 
//! This bypasses the bundler infrastructure and directly calls EntryPoint.handleOps()
//! 
//! Usage:
//!   cargo run --example submit_bundle -- \
//!     --bundler-key <private_key> \
//!     --sender <wallet_address> \
//!     --entry-point <entry_point_address> \
//!     --rpc-url <rpc_url>

use alloy_primitives::{keccak256, Address, Bytes, B256, U256, hex};
use alloy_sol_types::{SolCall, SolValue};
use clap::Parser;
use eyre::Result;

#[derive(Parser, Debug)]
#[command(name = "submit_bundle")]
#[command(about = "Submit a UserOperation bundle directly to EntryPoint")]
struct Args {
    /// Bundler's private key (for signing the transaction)
    #[arg(long)]
    bundler_key: String,

    /// Address of the smart contract wallet (sender)
    #[arg(long)]
    sender: Address,

    /// EntryPoint contract address
    #[arg(long)]
    entry_point: Address,

    /// RPC URL
    #[arg(long, default_value = "http://localhost:8545")]
    rpc_url: String,

    /// Wallet owner's private key (for signing the UserOp)
    #[arg(long)]
    owner_key: String,

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

    /// Beneficiary address (receives gas refund)
    #[arg(long)]
    beneficiary: Option<Address>,
}

// Define the EntryPoint handleOps function
alloy_sol_types::sol! {
    /// UserOperation struct (v0.6)
    #[derive(Debug)]
    struct UserOperationStruct {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        uint256 callGasLimit;
        uint256 verificationGasLimit;
        uint256 preVerificationGas;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        bytes paymasterAndData;
        bytes signature;
    }

    /// handleOps function
    #[derive(Debug)]
    function handleOps(
        UserOperationStruct[] calldata ops,
        address payable beneficiary
    ) external;
}

/// Calculate the user operation hash for v0.6
fn calculate_user_op_hash(
    sender: Address,
    nonce: U256,
    init_code: &Bytes,
    call_data: &Bytes,
    call_gas_limit: U256,
    verification_gas_limit: U256,
    pre_verification_gas: U256,
    max_fee_per_gas: U256,
    max_priority_fee_per_gas: U256,
    paymaster_and_data: &Bytes,
    entry_point: Address,
    chain_id: u64,
) -> B256 {
    // Hash dynamic fields
    let hash_init_code = keccak256(init_code);
    let hash_call_data = keccak256(call_data);
    let hash_paymaster_and_data = keccak256(paymaster_and_data);

    // Pack the user operation
    let packed = (
        sender,
        nonce,
        hash_init_code,
        hash_call_data,
        call_gas_limit,
        verification_gas_limit,
        pre_verification_gas,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        hash_paymaster_and_data,
    );

    // Hash the packed structure
    let packed_hash = keccak256(packed.abi_encode());

    // Add entry point and chain ID context
    let final_data = (packed_hash, entry_point, U256::from(chain_id));
    keccak256(final_data.abi_encode())
}

/// Sign a hash using a private key
fn sign_hash(hash: B256, private_key: &str) -> Result<Bytes> {
    use k256::ecdsa::SigningKey;
    
    // Parse private key
    let key_bytes = hex::decode(private_key.trim_start_matches("0x"))?;
    
    if key_bytes.len() != 32 {
        return Err(eyre::eyre!("Invalid private key length: expected 32 bytes, got {}", key_bytes.len()));
    }
    
    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&key_bytes);
    
    let signing_key = SigningKey::from_bytes(&key_array.into())?;
    
    // Add Ethereum signed message prefix
    // "\x19Ethereum Signed Message:\n32" + hash
    let prefix = b"\x19Ethereum Signed Message:\n32";
    let mut eth_message = Vec::with_capacity(prefix.len() + hash.len());
    eth_message.extend_from_slice(prefix);
    eth_message.extend_from_slice(&hash[..]);
    let prefixed_hash = keccak256(&eth_message);
    
    // Sign the prefixed hash
    let (sig, recid) = signing_key.sign_prehash_recoverable(&prefixed_hash[..])?;
    
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

    println!("🚀 Creating and Submitting UserOperation Bundle\n");

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

    // Gas parameters
    let call_gas_limit = U256::from(100000);
    let verification_gas_limit = U256::from(300000);
    let pre_verification_gas = U256::from(50000);
    let max_fee_per_gas = U256::from(2_000_000_000u64); // 2 gwei
    let max_priority_fee_per_gas = U256::from(1_000_000_000u64); // 1 gwei
    let nonce = U256::from(args.nonce);
    let paymaster_and_data = Bytes::new();

    // Calculate user operation hash
    let user_op_hash = calculate_user_op_hash(
        args.sender,
        nonce,
        &init_code,
        &call_data,
        call_gas_limit,
        verification_gas_limit,
        pre_verification_gas,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        &paymaster_and_data,
        args.entry_point,
        args.chain_id,
    );

    println!("📝 UserOperation Hash: 0x{}", hex::encode(user_op_hash));

    // Sign the UserOperation
    let signature = sign_hash(user_op_hash, &args.owner_key)?;
    println!("✅ Signature: 0x{}", hex::encode(&signature));
    println!();

    // Create UserOperation struct
    let user_op = UserOperationStruct {
        sender: args.sender,
        nonce,
        initCode: init_code,
        callData: call_data,
        callGasLimit: call_gas_limit,
        verificationGasLimit: verification_gas_limit,
        preVerificationGas: pre_verification_gas,
        maxFeePerGas: max_fee_per_gas,
        maxPriorityFeePerGas: max_priority_fee_per_gas,
        paymasterAndData: paymaster_and_data,
        signature,
    };

    // Beneficiary (who receives gas refund)
    let beneficiary = args.beneficiary.unwrap_or(
        args.sender // Default to sender as beneficiary
    );

    // Encode handleOps call
    let handle_ops_call = handleOpsCall {
        ops: vec![user_op],
        beneficiary,
    };

    let call_data = handle_ops_call.abi_encode();
    
    println!("📦 Bundle Details:");
    println!("   Operations: 1");
    println!("   Beneficiary: {}", beneficiary);
    println!("   EntryPoint: {}", args.entry_point);
    println!("   Call data size: {} bytes", call_data.len());
    println!();

    // Create the cast command
    let cast_cmd = format!(
        "cast send {} {} --private-key {} --rpc-url {} --gas-limit 3000000",
        args.entry_point,
        hex::encode(&call_data),
        args.bundler_key,
        args.rpc_url
    );

    println!("🔨 Submitting bundle to EntryPoint...");
    println!();
    println!("Cast command:");
    println!("{}", cast_cmd);
    println!();

    // Execute the command
    let output = std::process::Command::new("cast")
        .arg("send")
        .arg(args.entry_point.to_string())
        .arg(format!("0x{}", hex::encode(&call_data)))
        .arg("--private-key")
        .arg(&args.bundler_key)
        .arg("--rpc-url")
        .arg(&args.rpc_url)
        .arg("--gas-limit")
        .arg("3000000")
        .output()?;

    if output.status.success() {
        println!("✅ Bundle submitted successfully!");
        println!();
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("{}", stdout);
    } else {
        println!("❌ Bundle submission failed!");
        println!();
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("Error: {}", stderr);
        
        println!();
        println!("💡 You can also submit manually:");
        println!("   {}", cast_cmd);
    }

    Ok(())
}

