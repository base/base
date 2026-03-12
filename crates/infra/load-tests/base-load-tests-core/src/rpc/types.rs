use alloy_primitives::{Address, Bytes, U256};

/// Represents a transaction request to be submitted to the blockchain.
#[derive(Debug, Clone)]
pub struct TransactionRequest {
    /// Recipient address.
    pub to: Address,
    /// Value to transfer in wei.
    pub value: U256,
    /// Transaction data (contract call encoded data or empty for transfers).
    pub data: Bytes,
    /// Gas limit for execution.
    pub gas_limit: Option<u64>,
    /// Nonce to prevent replay attacks.
    pub nonce: Option<u64>,
}

impl TransactionRequest {
    /// Creates a transfer request with 21,000 gas limit.
    pub fn transfer(to: Address, value: U256) -> Self {
        Self { to, value, data: Bytes::default(), gas_limit: Some(21_000), nonce: None }
    }

    /// Creates a contract call request.
    pub const fn contract_call(to: Address, data: Bytes) -> Self {
        Self { to, value: U256::ZERO, data, gas_limit: None, nonce: None }
    }

    /// Sets the value field.
    pub const fn with_value(mut self, value: U256) -> Self {
        self.value = value;
        self
    }

    /// Sets the gas limit.
    pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    /// Sets the nonce.
    pub const fn with_nonce(mut self, nonce: u64) -> Self {
        self.nonce = Some(nonce);
        self
    }
}
