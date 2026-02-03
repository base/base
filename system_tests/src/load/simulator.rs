//! Simulator contract ABI bindings and bytecode.

use alloy_primitives::{Bytes, U160, U256};
use alloy_sol_types::SolCall;

alloy_sol_macro::sol! {
    struct PrecompileConfig {
        address precompile_address;
        uint256 num_calls;
    }

    struct SimulatorConfig {
        uint160 load_accounts;
        uint160 update_accounts;
        uint160 create_accounts;
        uint256 load_storage;
        uint256 update_storage;
        uint256 delete_storage;
        uint256 create_storage;
        PrecompileConfig[] precompiles;
    }

    interface Simulator {
        function run(SimulatorConfig calldata config) external;
        function initialize_storage_chunk() external;
        function initialize_address_chunk() external;
    }
}

const SIMULATOR_BYTECODE_HEX: &str = include_str!("simulator_bytecode.hex");

/// Encodes a call to `Simulator.run()` with the given config.
pub fn encode_run_call(config: &SimulatorConfig) -> Bytes {
    let call = Simulator::runCall { config: config.clone() };
    Bytes::from(call.abi_encode())
}

/// Builds a simulator config for creating storage slots and accounts.
pub fn build_simulator_config(create_storage: u64, create_accounts: u64) -> SimulatorConfig {
    SimulatorConfig {
        load_accounts: U160::ZERO,
        update_accounts: U160::ZERO,
        create_accounts: U160::from(create_accounts),
        load_storage: U256::ZERO,
        update_storage: U256::ZERO,
        delete_storage: U256::ZERO,
        create_storage: U256::from(create_storage),
        precompiles: vec![],
    }
}

/// Returns the Simulator contract deployment bytecode with constructor args.
pub fn simulator_deploy_bytecode(offset: u64) -> Bytes {
    let bytecode = hex::decode(SIMULATOR_BYTECODE_HEX.trim().trim_start_matches("0x"))
        .expect("invalid simulator bytecode hex");

    let offset_encoded = alloy_sol_types::SolValue::abi_encode(&U160::from(offset));

    let mut deploy_data = bytecode;
    deploy_data.extend_from_slice(&offset_encoded);

    Bytes::from(deploy_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_run_call() {
        let config = build_simulator_config(10, 5);
        let calldata = encode_run_call(&config);
        assert!(!calldata.is_empty());
        assert!(calldata.len() > 4);
    }

    #[test]
    fn test_deploy_bytecode() {
        let bytecode = simulator_deploy_bytecode(0);
        assert!(!bytecode.is_empty());
    }
}
