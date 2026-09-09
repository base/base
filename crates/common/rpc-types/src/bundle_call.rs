//! Bundle simulation RPC schemas.

use alloc::vec::Vec;

use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256};
use serde::{Deserialize, Serialize};

use crate::{TransactionIndex, u256_numeric_string};

/// Bundle of transactions for `eth_callBundle`
///
/// <https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_callBundle>
/// <https://github.com/flashbots/mev-geth/blob/fddf97beec5877483f879a77b7dea2e58a58d653/internal/ethapi/api.go#L2049>
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthCallBundle {
    /// A list of hex-encoded signed transactions
    pub txs: Vec<Bytes>,
    /// hex encoded block number for which this bundle is valid on
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,
    /// Either a hex encoded number or a block tag for which state to base this simulation on
    pub state_block_number: BlockNumberOrTag,
    /// Inclusive number of tx to replay in block. -1 means replay all
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_index: Option<TransactionIndex>,
    /// the coinbase to use for this bundle simulation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coinbase: Option<Address>,
    /// the timestamp to use for this bundle simulation, in seconds since the unix epoch
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    /// the timeout to apply to execution of this bundle, in milliseconds
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// gas limit of the block to use for this simulation
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub gas_limit: Option<u64>,
    /// difficulty of the block to use for this simulation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<U256>,
    /// basefee of the block to use for this simulation
    #[serde(default, with = "alloy_serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub base_fee: Option<u128>,
}

impl EthCallBundle {
    /// Creates a new bundle from the given [`Encodable2718`] transactions.
    pub fn from_2718<I, T>(txs: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Encodable2718,
    {
        Self::from_raw_txs(txs.into_iter().map(|tx| tx.encoded_2718()))
    }

    /// Creates a new bundle with the given transactions.
    pub fn from_raw_txs<I, T>(txs: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Bytes>,
    {
        Self { txs: txs.into_iter().map(Into::into).collect(), ..Default::default() }
    }

    /// Adds an [`Encodable2718`] transaction to the bundle.
    pub fn append_2718_tx(self, tx: impl Encodable2718) -> Self {
        self.append_raw_tx(tx.encoded_2718())
    }

    /// Adds an EIP-2718 envelope to the bundle.
    pub fn append_raw_tx(mut self, tx: impl Into<Bytes>) -> Self {
        self.txs.push(tx.into());
        self
    }

    /// Adds multiple [`Encodable2718`] transactions to the bundle.
    pub fn extend_2718_txs<I, T>(self, tx: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Encodable2718,
    {
        self.extend_raw_txs(tx.into_iter().map(|tx| tx.encoded_2718()))
    }

    /// Adds multiple calls to the block.
    pub fn extend_raw_txs<I, T>(mut self, txs: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Bytes>,
    {
        self.txs.extend(txs.into_iter().map(Into::into));
        self
    }

    /// Sets the block number for the bundle.
    pub const fn with_block_number(mut self, block_number: u64) -> Self {
        self.block_number = block_number;
        self
    }

    /// Sets the state block number for the bundle.
    pub fn with_state_block_number(
        mut self,
        state_block_number: impl Into<BlockNumberOrTag>,
    ) -> Self {
        self.state_block_number = state_block_number.into();
        self
    }

    /// Sets the coinbase for the bundle.
    pub const fn with_coinbase(mut self, coinbase: Address) -> Self {
        self.coinbase = Some(coinbase);
        self
    }

    /// Sets the timestamp for the bundle.
    pub const fn with_timestamp(mut self, timestamp: u64) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Sets the timeout for the bundle.
    pub const fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the gas limit for the bundle.
    pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    /// Sets the difficulty for the bundle.
    pub const fn with_difficulty(mut self, difficulty: U256) -> Self {
        self.difficulty = Some(difficulty);
        self
    }

    /// Sets the base fee for the bundle.
    pub const fn with_base_fee(mut self, base_fee: u128) -> Self {
        self.base_fee = Some(base_fee);
        self
    }
}

/// Response for `eth_callBundle`
///
/// <https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint#eth_callbundle>
/// <https://github.com/flashbots/mev-geth/blob/fddf97beec5877483f879a77b7dea2e58a58d653/internal/ethapi/api.go#L2212-L2220>
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EthCallBundleResponse {
    /// The hash of the bundle bodies.
    pub bundle_hash: B256,
    /// The gas price of the entire bundle
    #[serde(with = "u256_numeric_string")]
    pub bundle_gas_price: U256,
    /// The difference in Ether sent to the coinbase after all transactions in the bundle
    #[serde(with = "u256_numeric_string")]
    pub coinbase_diff: U256,
    /// The total amount of Ether sent to the coinbase after all transactions in the bundle
    #[serde(with = "u256_numeric_string")]
    pub eth_sent_to_coinbase: U256,
    /// The total gas fees paid for all transactions in the bundle
    #[serde(with = "u256_numeric_string")]
    pub gas_fees: U256,
    /// Results of individual transactions within the bundle
    pub results: Vec<EthCallBundleTransactionResult>,
    /// The block number used as a base for this simulation
    pub state_block_number: u64,
    /// The total gas used by all transactions in the bundle
    pub total_gas_used: u64,
}

/// Result of a single transaction in a bundle for `eth_callBundle`
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthCallBundleTransactionResult {
    /// The difference in Ether sent to the coinbase after the transaction
    #[serde(with = "u256_numeric_string")]
    pub coinbase_diff: U256,
    /// The amount of Ether sent to the coinbase after the transaction
    #[serde(with = "u256_numeric_string")]
    pub eth_sent_to_coinbase: U256,
    /// The address from which the transaction originated
    pub from_address: Address,
    /// The gas fees paid for the transaction
    #[serde(with = "u256_numeric_string")]
    pub gas_fees: U256,
    /// The gas price used for the transaction
    #[serde(with = "u256_numeric_string")]
    pub gas_price: U256,
    /// The amount of gas used by the transaction
    pub gas_used: u64,
    /// The address to which the transaction is sent (optional)
    pub to_address: Option<Address>,
    /// The transaction hash
    pub tx_hash: B256,
    /// Contains the return data if the transaction succeeded
    ///
    /// Note: this is mutually exclusive with `revert`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Bytes>,
    /// Contains the return data if the transaction reverted
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert: Option<Bytes>,
}

#[cfg(test)]
mod tests {

    use super::EthCallBundleResponse;

    #[test]
    fn can_deserialize_eth_call_resp() {
        let s = r#"{ "bundleGasPrice": "476190476193",
"bundleHash": "0x73b1e258c7a42fd0230b2fd05529c5d4b6fcb66c227783f8bece8aeacdd1db2e",
"coinbaseDiff": "20000000000126000",
"ethSentToCoinbase": "20000000000000000",
"gasFees": "126000",
"results": [
  {
    "coinbaseDiff": "10000000000063000",
    "ethSentToCoinbase": "10000000000000000",
    "fromAddress": "0x02a727155aef8609c9f7f2179b2a1f560b39f5a0",
    "gasFees": "63000",
    "gasPrice": "476190476193",
    "gasUsed": 21000,
    "toAddress": "0x73625f59cadc5009cb458b751b3e7b6b48c06f2c",
    "txHash": "0x669b4704a7d993a946cdd6e2f95233f308ce0c4649d2e04944e8299efcaa098a",
    "value": "0x"
  },
  {
    "coinbaseDiff": "10000000000063000",
    "ethSentToCoinbase": "10000000000000000",
    "fromAddress": "0x02a727155aef8609c9f7f2179b2a1f560b39f5a0",
    "gasFees": "63000",
    "gasPrice": "476190476193",
    "gasUsed": 21000,
    "toAddress": "0x73625f59cadc5009cb458b751b3e7b6b48c06f2c",
    "txHash": "0xa839ee83465657cac01adc1d50d96c1b586ed498120a84a64749c0034b4f19fa",
    "value": "0x"
  }
],
"stateBlockNumber": 5221585,
"totalGasUsed": 42000
}"#;

        let response = serde_json::from_str::<EthCallBundleResponse>(s).unwrap();
        let json: serde_json::Value = serde_json::from_str(s).unwrap();
        similar_asserts::assert_eq!(json, serde_json::to_value(response).unwrap());
    }
}
