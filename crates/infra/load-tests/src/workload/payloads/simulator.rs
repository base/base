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

/// Workload parameters for a single `Simulator.run()` call.
#[derive(Debug, Clone, Default)]
pub struct SimulatorWorkloadParams {
    /// Storage slots to SLOAD per call.
    pub load_storage: u64,
    /// Existing storage slots to SSTORE (update) per call.
    pub update_storage: u64,
    /// Storage slots to SSTORE to zero (delete) per call.
    pub delete_storage: u64,
    /// New storage slots to SSTORE (create) per call.
    pub create_storage: u64,
    /// Existing accounts to BALANCE-load per call.
    pub load_accounts: u64,
    /// Existing accounts to SEND to (update) per call.
    pub update_accounts: u64,
    /// New accounts to create (send to fresh address) per call.
    pub create_accounts: u64,
    /// Precompile addresses and call counts per transaction.
    pub precompile_calls: Vec<(Address, u64)>,
    /// Gas limit override per transaction.
    pub gas_limit: Option<u64>,
}

/// Generates transactions that call the `Simulator.run()` contract, exercising
/// synthetic EVM workloads (storage reads/writes, account operations, precompile calls).
#[derive(Debug, Clone)]
pub struct SimulatorPayload {
    contract: Option<Address>,
    params: SimulatorWorkloadParams,
}

impl SimulatorPayload {
    /// Creates a new simulator payload targeting the given deployed contract.
    pub const fn new(contract: Option<Address>, params: SimulatorWorkloadParams) -> Self {
        Self { contract, params }
    }

    /// Returns the contract address, if set.
    pub const fn contract(&self) -> Option<Address> {
        self.contract
    }

    /// Sets the contract address after CREATE deployment.
    pub const fn set_contract(&mut self, addr: Address) {
        self.contract = Some(addr);
    }

    /// Returns the per-call simulator parameters as a `SimulatorConfig`.
    pub fn simulator_config(&self) -> SimulatorConfig {
        SimulatorConfig {
            load_accounts: alloy_primitives::U160::from(self.params.load_accounts),
            update_accounts: alloy_primitives::U160::from(self.params.update_accounts),
            create_accounts: alloy_primitives::U160::from(self.params.create_accounts),
            load_storage: U256::from(self.params.load_storage),
            update_storage: U256::from(self.params.update_storage),
            delete_storage: U256::from(self.params.delete_storage),
            create_storage: U256::from(self.params.create_storage),
            precompiles: self
                .params
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
        let gas_limit = self.params.gas_limit.unwrap_or(DEFAULT_GAS_LIMIT);
        TransactionRequest::default()
            .with_to(contract)
            .with_input(self.encode_run_calldata())
            .with_gas_limit(gas_limit)
    }
}
