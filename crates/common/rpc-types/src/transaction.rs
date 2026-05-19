//! Types related to transactions for Base chains.

use alloy_consensus::{Transaction as TransactionTrait, Typed2718, transaction::Recovered};
use alloy_eips::{eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{Address, B256, BlockHash, Bytes, ChainId, TxKind, U256};
use alloy_serde::OtherFields;
use base_common_consensus::{BaseTransactionInfo, BaseTxEnvelope};
use serde::{Deserialize, Serialize};

mod request;
pub use request::BaseTransactionRequest;

/// Base transaction type
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, derive_more::Deref, derive_more::DerefMut,
)]
#[cfg_attr(all(any(test, feature = "arbitrary"), feature = "k256"), derive(arbitrary::Arbitrary))]
#[serde(try_from = "tx_serde::TransactionSerdeHelper", into = "tx_serde::TransactionSerdeHelper")]
pub struct Transaction {
    /// Ethereum Transaction Types
    #[deref]
    #[deref_mut]
    pub inner: alloy_rpc_types_eth::Transaction<BaseTxEnvelope>,

    /// Nonce for deposit transactions. Only present in RPC responses.
    pub deposit_nonce: Option<u64>,

    /// Deposit receipt version for deposit transactions post-canyon
    pub deposit_receipt_version: Option<u64>,
}

impl Transaction {
    /// Converts a consensus `tx` with an additional context `tx_info` into an RPC [`Transaction`].
    pub fn from_transaction(tx: Recovered<BaseTxEnvelope>, tx_info: BaseTransactionInfo) -> Self {
        let base_fee = tx_info.inner.base_fee;
        let effective_gas_price = if tx.is_deposit() {
            // For deposits, we must always set the `gasPrice` field to 0 in rpc
            // deposit tx don't have a gas price field, but serde of `Transaction` will take care of
            // it
            0
        } else {
            base_fee
                .map(|base_fee| {
                    tx.effective_tip_per_gas(base_fee).unwrap_or_default() + base_fee as u128
                })
                .unwrap_or_else(|| tx.max_fee_per_gas())
        };

        Self {
            inner: alloy_rpc_types_eth::Transaction {
                inner: tx,
                block_hash: tx_info.inner.block_hash,
                block_number: tx_info.inner.block_number,
                transaction_index: tx_info.inner.index,
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce: tx_info.deposit_meta.deposit_nonce,
            deposit_receipt_version: tx_info.deposit_meta.deposit_receipt_version,
        }
    }
}

impl Typed2718 for Transaction {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

impl TransactionTrait for Transaction {
    fn chain_id(&self) -> Option<ChainId> {
        self.inner.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.inner.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.inner.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.inner.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.inner.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.inner.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn is_create(&self) -> bool {
        self.inner.is_create()
    }

    fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.inner.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.inner.authorization_list()
    }
}

impl alloy_network_primitives::TransactionResponse for Transaction {
    fn tx_hash(&self) -> alloy_primitives::TxHash {
        self.inner.tx_hash()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash()
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number()
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn from(&self) -> Address {
        self.inner.from()
    }
}

/// Base chain-specific transaction fields
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseTransactionFields {
    /// The ETH value to mint on L2
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub mint: Option<u128>,
    /// Hash that uniquely identifies the source of the deposit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<B256>,
    /// Field indicating whether the transaction is a system transaction, and therefore
    /// exempt from the L2 gas limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_system_tx: Option<bool>,
    /// Deposit receipt version for deposit transactions post-canyon
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub deposit_receipt_version: Option<u64>,
}

impl TryFrom<BaseTransactionFields> for OtherFields {
    type Error = serde_json::Error;

    fn try_from(value: BaseTransactionFields) -> Result<Self, Self::Error> {
        serde_json::to_value(value)?.try_into()
    }
}

impl AsRef<BaseTxEnvelope> for Transaction {
    fn as_ref(&self) -> &BaseTxEnvelope {
        self.inner.as_ref()
    }
}

mod tx_serde {
    //! Helper module for serializing and deserializing Base [`Transaction`].
    //!
    //! This is needed because we might need to deserialize the `from` field into both
    //! [`alloy_consensus::transaction::Recovered::signer`] which resides in
    //! [`alloy_rpc_types_eth::Transaction::inner`] and [`base_common_consensus::TxDeposit::from`].
    //!
    //! Additionally, we need similar logic for the `gasPrice` field
    use alloy_consensus::{Transaction as TransactionTrait, transaction::Recovered};
    use base_common_consensus::BaseTxEnvelope;
    use serde::{Deserialize, Serialize, de::Error};

    use super::{Address, BlockHash, Transaction};

    /// Helper struct which will be flattened into the transaction and will only contain `from`
    /// field if inner [`BaseTxEnvelope`] did not consume it.
    #[derive(Serialize, Deserialize)]
    struct OptionalFields {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Address>,
        #[serde(
            default,
            rename = "gasPrice",
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )]
        effective_gas_price: Option<u128>,
        #[serde(
            default,
            rename = "nonce",
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )]
        deposit_nonce: Option<u64>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct TransactionSerdeHelper {
        #[serde(flatten)]
        inner: BaseTxEnvelope,
        #[serde(default)]
        block_hash: Option<BlockHash>,
        #[serde(default, with = "alloy_serde::quantity::opt")]
        block_number: Option<u64>,
        #[serde(default, with = "alloy_serde::quantity::opt")]
        transaction_index: Option<u64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )]
        deposit_receipt_version: Option<u64>,

        #[serde(flatten)]
        other: OptionalFields,
    }

    impl From<Transaction> for TransactionSerdeHelper {
        fn from(value: Transaction) -> Self {
            let Transaction {
                inner:
                    alloy_rpc_types_eth::Transaction {
                        inner,
                        block_hash,
                        block_number,
                        transaction_index,
                        effective_gas_price,
                    },
                deposit_receipt_version,
                deposit_nonce,
            } = value;

            // Deposit and EIP-8130 transactions already serialize their own `from` field through
            // the inner envelope, so avoid emitting it twice via `OptionalFields`.
            let from =
                if matches!(inner.inner(), BaseTxEnvelope::Deposit(_) | BaseTxEnvelope::Eip8130(_))
                {
                    None
                } else {
                    Some(inner.signer())
                };

            // if inner transaction has its own `gasPrice` don't serialize it in this struct.
            let effective_gas_price = effective_gas_price.filter(|_| inner.gas_price().is_none());

            Self {
                inner: inner.into_inner(),
                block_hash,
                block_number,
                transaction_index,
                deposit_receipt_version,
                other: OptionalFields { from, effective_gas_price, deposit_nonce },
            }
        }
    }

    impl TryFrom<TransactionSerdeHelper> for Transaction {
        type Error = serde_json::Error;

        fn try_from(value: TransactionSerdeHelper) -> Result<Self, Self::Error> {
            let TransactionSerdeHelper {
                inner,
                block_hash,
                block_number,
                transaction_index,
                deposit_receipt_version,
                other,
            } = value;

            // Try to get `from` field from inner envelope or from `MaybeFrom`, otherwise return
            // error
            let from = if let Some(from) = other.from {
                from
            } else {
                inner
                    .as_deposit()
                    .map(|v| v.from)
                    .ok_or_else(|| serde_json::Error::custom("missing `from` field"))?
            };

            // Only serialize deposit_nonce if inner transaction is deposit to avoid duplicated keys
            let deposit_nonce = other.deposit_nonce.filter(|_| inner.is_deposit());

            let effective_gas_price = other.effective_gas_price.or_else(|| inner.gas_price());

            Ok(Self {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: Recovered::new_unchecked(inner, from),
                    block_hash,
                    block_number,
                    transaction_index,
                    effective_gas_price,
                },
                deposit_receipt_version,
                deposit_nonce,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_deserialize_deposit() {
        // cast rpc eth_getTransactionByHash
        // 0xbc9329afac05556497441e2b3ee4c5d4da7ca0b2a4c212c212d0739e94a24df9
        let rpc_tx = r#"{"blockHash":"0x9d86bb313ebeedf4f9f82bf8a19b426be656a365648a7c089b618771311db9f9","blockNumber":"0x798ad0b","hash":"0xbc9329afac05556497441e2b3ee4c5d4da7ca0b2a4c212c212d0739e94a24df9","transactionIndex":"0x0","type":"0x7e","nonce":"0x152ea95","input":"0x440a5e200000146b000f79c50000000000000003000000006725333f000000000141e287000000000000000000000000000000000000000000000000000000012439ee7e0000000000000000000000000000000000000000000000000000000063f363e973e96e7145ff001c81b9562cba7b6104eeb12a2bc4ab9f07c27d45cd81a986620000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985","mint":"0x0","sourceHash":"0x04e9a69416471ead93b02f0c279ab11ca0b635db5c1726a56faf22623bafde52","r":"0x0","s":"0x0","v":"0x0","yParity":"0x0","gas":"0xf4240","from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001","to":"0x4200000000000000000000000000000000000015","depositReceiptVersion":"0x1","value":"0x0","gasPrice":"0x0"}"#;

        let tx = serde_json::from_str::<Transaction>(rpc_tx).unwrap();

        let BaseTxEnvelope::Deposit(inner) = tx.as_ref() else {
            panic!("Expected deposit transaction");
        };
        assert_eq!(tx.inner.inner.signer(), inner.from);
        assert_eq!(tx.deposit_nonce, Some(22211221));
        assert_eq!(tx.inner.effective_gas_price, Some(0));

        let deserialized = serde_json::to_value(&tx).unwrap();
        let expected = serde_json::from_str::<serde_json::Value>(rpc_tx).unwrap();
        similar_asserts::assert_eq!(deserialized, expected);
    }

    #[test]
    fn can_deserialize_eip8130() {
        let rpc_tx = r#"{
            "type":"0x7b",
            "chainId":"0x509f455",
            "from":"0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc",
            "nonceKey":"0x0",
            "nonceSequence":"0x0",
            "expiry":"0x0",
            "maxPriorityFeePerGas":"0xf4240",
            "maxFeePerGas":"0x7744d640",
            "gas":"0x14751",
            "accountChanges":[],
            "calls":[[{"to":"0x8464135c8f25da09e49bc8782676a84730c318bc","data":"0xb74af5a9"}]],
            "payer":null,
            "senderAuth":"0x0000000000000000000000000000000000000001f654c19d8a7e70cd2da73f22c5568ebbd33c90b19554e621591c1b7ecea02413327ec3f23f17a03343fb6f2ad69ddad99a507f4d80d082c9173db7ccb06ce3cf1b",
            "payerAuth":"0x",
            "hash":"0xe05ee7338ea863d0a7d6b1eef28a0baf7c1e21ef1367541b005aff98401014f5",
            "blockHash":"0xcce44084adef30124842b2929b14886714cba8a847a0d5d0714a712e87378f9e",
            "blockNumber":"0x6f",
            "transactionIndex":"0x1",
            "gasPrice":"0x3baa0c40"
        }"#;

        let tx = serde_json::from_str::<Transaction>(rpc_tx).unwrap();
        assert!(matches!(tx.inner.inner.inner(), BaseTxEnvelope::Eip8130(_)));
        assert_eq!(tx.inner.block_number, Some(111));
    }

    #[test]
    fn can_deserialize_block_with_eip8130() {
        let rpc_block = r#"{
            "hash":"0xcce44084adef30124842b2929b14886714cba8a847a0d5d0714a712e87378f9e",
            "parentHash":"0xc52c8b3fc23390387c53c1aba7b26b4b651185ede66d9e3fadc2fde86cf68838",
            "sha3Uncles":"0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner":"0x4200000000000000000000000000000000000011",
            "stateRoot":"0x3b6f9a770147923771e34f1dbbf1a4238f124ae2be28a692a6f74d3b43ea8580",
            "transactionsRoot":"0xaaabc059829430a46c757b9f0ddc94447fa8de267a7900f8b89fc731a2d98de2",
            "receiptsRoot":"0x0b9c48d0d95f82a06cc545bb538414924a9eb97a2f0a79a1fe1a8c5554afaf78",
            "logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "difficulty":"0x0",
            "number":"0x6f",
            "gasLimit":"0x3938700",
            "gasUsed":"0x10656",
            "timestamp":"0x6a0c48f6",
            "extraData":"0x01000000fa00000006000000003b9aca00",
            "mixHash":"0x854e704bf06a11be05732f16e54e06c8578da42993717887d0b4596dc8935240",
            "nonce":"0x0000000000000000",
            "baseFeePerGas":"0x3b9aca00",
            "withdrawalsRoot":"0x8ed4baae3a927be3dea54996b4d5899f8c01e7594bf50b17dc1e741388ce3d12",
            "blobGasUsed":"0x9c40",
            "excessBlobGas":"0x0",
            "parentBeaconBlockRoot":"0xcd567f3b26140759d5b09a4ffe9e45cc6cce2769065aaae23f7a0cc34a60010c",
            "requestsHash":"0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "size":"0x433",
            "uncles":[],
            "transactions":[
                {
                    "type":"0x7e",
                    "sourceHash":"0xd48711d3aa712dc0aa9bb0dacd1bf6387d48d7a26d719c8c5cffb6da7e55ff5f",
                    "from":"0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
                    "to":"0x4200000000000000000000000000000000000015",
                    "mint":"0x0",
                    "value":"0x0",
                    "gas":"0xf4240",
                    "input":"0x3db6be2b00000558000c3c9d0000000000000001000000006a0c48f00000000000000046000000000000000000000000000000000000000000000000000000000002b7210000000000000000000000000000000000000000000000000000000000000001af26f0ae17f7ee6b18c42262bdeaf05e6b169a11d0292aae63e518907b485fc9000000000000000000000000976ea74026e726554db657fa54763abd0c3a0aa90000000000000000000000000190",
                    "hash":"0x7cd4f7a713672f424ddb7f49e908eb7e034ed359d90f0b2053e313b5865c3365",
                    "r":"0x0",
                    "s":"0x0",
                    "yParity":"0x0",
                    "v":"0x0",
                    "blockHash":"0xcce44084adef30124842b2929b14886714cba8a847a0d5d0714a712e87378f9e",
                    "blockNumber":"0x6f",
                    "transactionIndex":"0x0",
                    "depositReceiptVersion":"0x1",
                    "gasPrice":"0x0",
                    "nonce":"0x6e"
                },
                {
                    "type":"0x7b",
                    "chainId":"0x509f455",
                    "from":"0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc",
                    "nonceKey":"0x0",
                    "nonceSequence":"0x0",
                    "expiry":"0x0",
                    "maxPriorityFeePerGas":"0xf4240",
                    "maxFeePerGas":"0x7744d640",
                    "gas":"0x14751",
                    "accountChanges":[],
                    "calls":[[{"to":"0x8464135c8f25da09e49bc8782676a84730c318bc","data":"0xb74af5a9"}]],
                    "payer":null,
                    "senderAuth":"0x0000000000000000000000000000000000000001f654c19d8a7e70cd2da73f22c5568ebbd33c90b19554e621591c1b7ecea02413327ec3f23f17a03343fb6f2ad69ddad99a507f4d80d082c9173db7ccb06ce3cf1b",
                    "payerAuth":"0x",
                    "hash":"0xe05ee7338ea863d0a7d6b1eef28a0baf7c1e21ef1367541b005aff98401014f5",
                    "blockHash":"0xcce44084adef30124842b2929b14886714cba8a847a0d5d0714a712e87378f9e",
                    "blockNumber":"0x6f",
                    "transactionIndex":"0x1",
                    "gasPrice":"0x3baa0c40"
                }
            ],
            "withdrawals":[]
        }"#;

        let block = serde_json::from_str::<
            alloy_rpc_types_eth::Block<Transaction, alloy_rpc_types_eth::Header>,
        >(rpc_block)
        .unwrap();
        assert_eq!(block.header.number, 111);
        assert_eq!(block.transactions.txns().count(), 2);
    }

    #[test]
    fn eip8130_serialization_emits_single_from_field() {
        let rpc_tx = r#"{
            "type":"0x7b",
            "chainId":"0x509f455",
            "from":"0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc",
            "nonceKey":"0x0",
            "nonceSequence":"0x0",
            "expiry":"0x0",
            "maxPriorityFeePerGas":"0xf4240",
            "maxFeePerGas":"0x7744d640",
            "gas":"0x14751",
            "accountChanges":[],
            "calls":[[{"to":"0x8464135c8f25da09e49bc8782676a84730c318bc","data":"0xb74af5a9"}]],
            "payer":null,
            "senderAuth":"0x0000000000000000000000000000000000000001f654c19d8a7e70cd2da73f22c5568ebbd33c90b19554e621591c1b7ecea02413327ec3f23f17a03343fb6f2ad69ddad99a507f4d80d082c9173db7ccb06ce3cf1b",
            "payerAuth":"0x",
            "hash":"0xe05ee7338ea863d0a7d6b1eef28a0baf7c1e21ef1367541b005aff98401014f5",
            "blockHash":"0xcce44084adef30124842b2929b14886714cba8a847a0d5d0714a712e87378f9e",
            "blockNumber":"0x6f",
            "transactionIndex":"0x1",
            "gasPrice":"0x3baa0c40"
        }"#;

        let tx = serde_json::from_str::<Transaction>(rpc_tx).unwrap();
        let serialized = serde_json::to_string(&tx).unwrap();
        assert_eq!(serialized.matches("\"from\"").count(), 1);
    }
}
