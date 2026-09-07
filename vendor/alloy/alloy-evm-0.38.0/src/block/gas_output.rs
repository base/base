/// Gas used by a transaction, split into regular and state gas components.
///
/// EIP-8037 introduces dual-limit gas accounting with a separate state gas reservation
/// that tracks gas spent on state creation operations (SSTORE, CREATE, account creation, code
/// deposit).
///
/// - State gas: State-specific gas tracking (storage and contract creation)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GasOutput {
    /// Gas used by the transaction. This value is find in Receipt.
    tx_gas_used: u64,
    /// State gas used by the transaction.
    state_gas_used: u64,
}

impl GasOutput {
    /// Creates a new `GasOutput` with the given regular gas used.
    pub const fn new(tx_gas_used: u64) -> Self {
        Self { tx_gas_used, state_gas_used: 0 }
    }

    /// Creates a new `GasOutput` with both regular and state gas.
    pub const fn with_state_gas(tx_gas_used: u64, state_gas_used: u64) -> Self {
        Self { tx_gas_used, state_gas_used }
    }

    /// Returns the regular gas used (execution gas).
    pub const fn tx_gas_used(&self) -> u64 {
        self.tx_gas_used
    }

    /// Returns the state gas used (gas for state creation operations).
    /// Only non-zero when Amsterdam is active.
    pub const fn state_gas_used(&self) -> u64 {
        self.state_gas_used
    }
}
