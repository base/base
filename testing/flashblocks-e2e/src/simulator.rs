//! Simulator contract bindings for resource usage testing.
//!
//! The Simulator contract from base-benchmark enables configurable resource
//! consumption for testing bundle metering. It can create storage slots,
//! accounts, and call precompiles to generate different types of load.
//!
//! This is useful for testing:
//! - `state_root_time_us`: Create accounts/storage to increase state root time
//! - `total_execution_time_us`: Call precompiles to increase execution time
//! - `total_gas_used`: Storage operations consume gas

use alloy_primitives::{Address, Bytes, U160, U256};
use alloy_sol_types::SolCall;

// Define the Simulator contract ABI using alloy's sol! macro
alloy_sol_macro::sol! {
    /// Configuration for precompile calls.
    struct PrecompileConfig {
        address precompile_address;
        uint256 num_calls;
    }

    /// Configuration for the Simulator.run() call.
    ///
    /// Each field controls a different type of resource consumption:
    /// - `load_accounts`: Read existing accounts (minimal impact)
    /// - `update_accounts`: Update existing account balances
    /// - `create_accounts`: Create new accounts (increases state root time)
    /// - `load_storage`: Read existing storage slots
    /// - `update_storage`: Update existing storage slots
    /// - `delete_storage`: Delete storage slots
    /// - `create_storage`: Create new storage slots (gas pressure)
    /// - `precompiles`: Call precompiles (execution time)
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

    /// The Simulator contract interface.
    interface Simulator {
        /// Execute the configured resource operations.
        function run(SimulatorConfig calldata config) external;
        /// Initialize a chunk of storage slots for later use.
        function initialize_storage_chunk() external;
        /// Initialize a chunk of addresses for later use.
        function initialize_address_chunk() external;
    }
}

/// Encode the Simulator.run() call with the given configuration.
pub fn encode_run_call(config: &SimulatorConfig) -> Bytes {
    let call = Simulator::runCall { config: config.clone() };
    Bytes::from(call.abi_encode())
}

/// Builder for SimulatorConfig with sensible defaults.
#[derive(Debug, Clone, Default)]
pub struct SimulatorConfigBuilder {
    create_accounts: u64,
    create_storage: u64,
    precompile_calls: u64,
    precompile_address: Option<Address>,
}

impl SimulatorConfigBuilder {
    /// Create a new builder with all zeros.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the number of accounts to create.
    ///
    /// Creating accounts increases state root computation time.
    pub const fn create_accounts(mut self, count: u64) -> Self {
        self.create_accounts = count;
        self
    }

    /// Set the number of storage slots to create.
    ///
    /// Creating storage increases gas usage.
    pub const fn create_storage(mut self, count: u64) -> Self {
        self.create_storage = count;
        self
    }

    /// Set precompile calls for execution time testing.
    pub const fn precompile_calls(mut self, count: u64, address: Address) -> Self {
        self.precompile_calls = count;
        self.precompile_address = Some(address);
        self
    }

    /// Build the SimulatorConfig.
    pub fn build(self) -> SimulatorConfig {
        let precompiles = if self.precompile_calls > 0 {
            self.precompile_address.map_or_else(Vec::new, |addr| {
                vec![PrecompileConfig {
                    precompile_address: addr,
                    num_calls: U256::from(self.precompile_calls),
                }]
            })
        } else {
            vec![]
        };

        SimulatorConfig {
            load_accounts: U160::ZERO,
            update_accounts: U160::ZERO,
            create_accounts: U160::from(self.create_accounts),
            load_storage: U256::ZERO,
            update_storage: U256::ZERO,
            delete_storage: U256::ZERO,
            create_storage: U256::from(self.create_storage),
            precompiles,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_run_call() {
        let config = SimulatorConfigBuilder::new().create_accounts(5).create_storage(10).build();

        let calldata = encode_run_call(&config);
        assert!(!calldata.is_empty());
        // The function selector for run(SimulatorConfig) should be at the start
        assert!(calldata.len() > 4);
    }

    #[test]
    fn test_builder_defaults() {
        let config = SimulatorConfigBuilder::new().build();
        assert_eq!(config.create_accounts, U160::ZERO);
        assert_eq!(config.create_storage, U256::ZERO);
        assert!(config.precompiles.is_empty());
    }
}
