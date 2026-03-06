use alloy_consensus::ReceiptEnvelope;
use alloy_rpc_types_eth::TransactionReceipt;

/// Receipt conversion utilities for TEE proving.
#[derive(Debug)]
pub struct ReceiptConverter;

impl ReceiptConverter {
    /// Converts RPC transaction receipts to consensus receipt envelopes.
    ///
    /// Maps `ReceiptEnvelope<rpc::Log>` to `ReceiptEnvelope<primitives::Log>`
    /// by extracting the inner log data.
    pub fn convert_receipts(receipts: Vec<TransactionReceipt>) -> Vec<ReceiptEnvelope> {
        receipts
            .into_iter()
            .map(|r| {
                r.inner.map_logs(|log| alloy_primitives::Log {
                    address: log.inner.address,
                    data: log.inner.data,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Receipt, ReceiptWithBloom};
    use alloy_primitives::{Bytes, LogData, address};

    use super::*;

    #[test]
    fn test_convert_receipts_preserves_log_data() {
        let log_address = address!("2222222222222222222222222222222222222222");
        let log_data = LogData::new_unchecked(vec![], Bytes::from(vec![0x42]));
        let rpc_log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log { address: log_address, data: log_data.clone() },
            ..Default::default()
        };

        let receipt =
            Receipt { status: true.into(), cumulative_gas_used: 21000, logs: vec![rpc_log] };
        let envelope =
            ReceiptEnvelope::Legacy(ReceiptWithBloom { receipt, logs_bloom: Default::default() });

        let tx_receipt = alloy_rpc_types_eth::TransactionReceipt {
            inner: envelope,
            transaction_hash: alloy_primitives::B256::ZERO,
            transaction_index: Some(0),
            block_hash: None,
            block_number: None,
            gas_used: 21000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: alloy_primitives::Address::ZERO,
            to: None,
            contract_address: None,
        };

        let converted = ReceiptConverter::convert_receipts(vec![tx_receipt]);
        assert_eq!(converted.len(), 1);

        match &converted[0] {
            ReceiptEnvelope::Legacy(rwb) => {
                assert_eq!(rwb.receipt.logs.len(), 1);
                assert_eq!(rwb.receipt.logs[0].address, log_address);
                assert_eq!(rwb.receipt.logs[0].data, log_data);
            }
            _ => panic!("expected Legacy receipt envelope"),
        }
    }
}
