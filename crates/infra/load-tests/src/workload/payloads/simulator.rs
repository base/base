use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, SolValue, sol};

use super::Payload;
use crate::workload::SeededRng;

sol! {
    #[allow(missing_docs)]
    struct PrecompileConfig {
        address precompile_address;
        uint256 num_calls;
    }

    #[allow(missing_docs)]
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

    #[allow(missing_docs)]
    interface ISimulator {
        function run(SimulatorConfig calldata config) external;
        function initialize_storage_chunk() external;
        function initialize_address_chunk() external;
        function num_storage_slots_needed(SimulatorConfig calldata config) external view returns (uint256);
        function num_accounts_needed(SimulatorConfig calldata config) external view returns (uint160);
        function num_storage_initialized() external view returns (uint256);
        function num_address_initialized() external view returns (uint160);
    }
}

const DEFAULT_GAS_LIMIT: u64 = 30_000_000;

/// Generates transactions that call the `Simulator.run()` contract, exercising
/// synthetic EVM workloads (storage reads/writes, account operations, precompile calls).
#[derive(Debug, Clone)]
pub struct SimulatorPayload {
    contract: Option<Address>,
    load_storage: u64,
    update_storage: u64,
    delete_storage: u64,
    create_storage: u64,
    load_accounts: u64,
    update_accounts: u64,
    create_accounts: u64,
    precompile_calls: Vec<(Address, u64)>,
    gas_limit: u64,
}

impl SimulatorPayload {
    /// Creates a new simulator payload targeting the given deployed contract.
    pub fn new(
        contract: Option<Address>,
        load_storage: u64,
        update_storage: u64,
        delete_storage: u64,
        create_storage: u64,
        load_accounts: u64,
        update_accounts: u64,
        create_accounts: u64,
        precompile_calls: Vec<(Address, u64)>,
        gas_limit: Option<u64>,
    ) -> Self {
        Self {
            contract,
            load_storage,
            update_storage,
            delete_storage,
            create_storage,
            load_accounts,
            update_accounts,
            create_accounts,
            precompile_calls,
            gas_limit: gas_limit.unwrap_or(DEFAULT_GAS_LIMIT),
        }
    }

    /// Returns the contract address, if set.
    pub fn contract(&self) -> Option<Address> {
        self.contract
    }

    /// Sets the contract address after CREATE2 deployment.
    pub fn set_contract(&mut self, addr: Address) {
        self.contract = Some(addr);
    }

    /// Returns the per-call simulator parameters as a `SimulatorConfig`.
    pub fn simulator_config(&self) -> SimulatorConfig {
        SimulatorConfig {
            load_accounts: alloy_primitives::U160::from(self.load_accounts),
            update_accounts: alloy_primitives::U160::from(self.update_accounts),
            create_accounts: alloy_primitives::U160::from(self.create_accounts),
            load_storage: U256::from(self.load_storage),
            update_storage: U256::from(self.update_storage),
            delete_storage: U256::from(self.delete_storage),
            create_storage: U256::from(self.create_storage),
            precompiles: self
                .precompile_calls
                .iter()
                .map(|(addr, count)| PrecompileConfig {
                    precompile_address: *addr,
                    num_calls: U256::from(*count),
                })
                .collect(),
        }
    }

    /// ABI-encodes `initialize_storage_chunk()`.
    pub fn encode_initialize_storage_chunk() -> Bytes {
        Bytes::from(ISimulator::initialize_storage_chunkCall {}.abi_encode())
    }

    /// ABI-encodes `initialize_address_chunk()`.
    pub fn encode_initialize_address_chunk() -> Bytes {
        Bytes::from(ISimulator::initialize_address_chunkCall {}.abi_encode())
    }

    /// ABI-encodes `num_storage_slots_needed(config)`.
    pub fn encode_num_storage_slots_needed(config: SimulatorConfig) -> Bytes {
        Bytes::from(ISimulator::num_storage_slots_neededCall { config }.abi_encode())
    }

    /// ABI-encodes `num_accounts_needed(config)`.
    pub fn encode_num_accounts_needed(config: SimulatorConfig) -> Bytes {
        Bytes::from(ISimulator::num_accounts_neededCall { config }.abi_encode())
    }

    /// ABI-encodes `num_storage_initialized()`.
    pub fn encode_num_storage_initialized() -> Bytes {
        Bytes::from(ISimulator::num_storage_initializedCall {}.abi_encode())
    }

    /// ABI-encodes `num_address_initialized()`.
    pub fn encode_num_address_initialized() -> Bytes {
        Bytes::from(ISimulator::num_address_initializedCall {}.abi_encode())
    }

    /// Decodes a `uint256` ABI return value (used for slot counts).
    pub fn decode_u256_return(bytes: &[u8]) -> Option<U256> {
        U256::abi_decode(bytes).ok()
    }

    fn encode_run_calldata(&self) -> Bytes {
        Bytes::from(ISimulator::runCall { config: self.simulator_config() }.abi_encode())
    }
}

impl Payload for SimulatorPayload {
    fn name(&self) -> &'static str {
        "simulator"
    }

    fn generate(&self, _rng: &mut SeededRng, _from: Address, _to: Address) -> TransactionRequest {
        let contract = self.contract.unwrap_or(Address::ZERO);
        TransactionRequest::default()
            .with_to(contract)
            .with_input(self.encode_run_calldata())
            .with_gas_limit(self.gas_limit)
    }
}
