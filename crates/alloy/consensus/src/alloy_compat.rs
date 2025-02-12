//! Additional compatibility implementations.

use crate::{TxDeposit, DEPOSIT_TX_TYPE_ID};
use alloc::string::ToString;
use alloy_eips::Typed2718;
use alloy_network::{UnknownTxEnvelope, UnknownTypedTransaction};
use alloy_rpc_types_eth::ConversionError;

impl TryFrom<UnknownTxEnvelope> for TxDeposit {
    type Error = ConversionError;

    fn try_from(value: UnknownTxEnvelope) -> Result<Self, Self::Error> {
        value.inner.try_into()
    }
}

impl TryFrom<UnknownTypedTransaction> for TxDeposit {
    type Error = ConversionError;

    fn try_from(value: UnknownTypedTransaction) -> Result<Self, Self::Error> {
        if !value.is_type(DEPOSIT_TX_TYPE_ID) {
            return Err(ConversionError::Custom("invalid transaction type".to_string()));
        }
        value
            .fields
            .deserialize_into()
            .map_err(|_| ConversionError::Custom("invalid transaction data".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversion() {
        let deposit = r#"{
  "blockHash": "0x2c475c5d2d609929cec7be9caaaebd29be53e4ef21b1f7b897cb954469e20d01",
  "blockNumber": "0x191350d",
  "depositReceiptVersion": "0x1",
  "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
  "gas": "0xf4240",
  "gasPrice": "0x0",
  "hash": "0x096c03d72acb06339c9c7860d1c36b6451932ec0ff16fd34aa9e30a73a245e13",
  "input": "0x440a5e20000008dd00101c1200000000000000030000000067acc63f00000000014d1f2d000000000000000000000000000000000000000000000000000000005ba4c0eb00000000000000000000000000000000000000000000000000000001ce2291bdcbb8f62c15343b39cfacdbf81c4747822ebb16c2518126e47d984422a82defc10000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9",
  "mint": "0x0",
  "nonce": "0x191350e",
  "r": "0x0",
  "s": "0x0",
  "sourceHash": "0x990d7122a1f121f3a6bc45723e28f4921c269037a77e77ffee3c8585136d1a92",
  "to": "0x4200000000000000000000000000000000000015",
  "transactionIndex": "0x0",
  "type": "0x7e",
  "v": "0x0",
  "value": "0x0"
}"#;

        let unknown_tx_envelope: UnknownTxEnvelope = serde_json::from_str(deposit).unwrap();

        let _deposit: TxDeposit = unknown_tx_envelope.try_into().unwrap();
    }
}
