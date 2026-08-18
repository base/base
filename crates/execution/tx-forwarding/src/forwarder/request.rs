//! What a forwarder sends: one JSON-RPC call to one destination.

use alloy_primitives::TxHash;
use base_execution_txpool::{NoExtensions, ValidatedTransaction};
use jsonrpsee::core::params::ArrayParams;
use serde::Serialize;

/// One JSON-RPC call relayed to one forwarding destination.
///
/// This is the only thing a [`DestinationForwarder`](super::DestinationForwarder) knows about what
/// it carries. Batching, rate limiting, retries, metrics and shutdown are all expressed over this
/// trait, so a producer that is not the transaction pool can drive the same transport.
///
/// This crate only ever names [`InsertValidatedTransaction`]. Downstream node builds implement it
/// on their own message type to relay additional methods over one ordered per-destination queue.
///
/// `Sync` is required because the forwarder holds a borrow of its buffer across the await on an
/// in-flight batch.
pub trait ForwardRequest: Send + Sync + 'static {
    /// JSON-RPC method this call invokes.
    ///
    /// Read per request rather than per batch, so one batch may mix methods. Batch entries are
    /// executed in order by the server, which is what lets a producer rely on submission order
    /// between different kinds of call to the same destination.
    fn method(&self) -> &'static str;

    /// Positional parameters for the call.
    ///
    /// Fallible because serialization is: a producer that hands over an unserializable payload must
    /// cost its batch, not panic the forwarder task.
    fn params(&self) -> Result<ArrayParams, serde_json::Error>;

    /// Transaction this call concerns, for metrics and transaction events.
    ///
    /// [`None`] for a call not attributable to a single transaction.
    fn tx_hash(&self) -> Option<TxHash>;
}

/// A `base_insertValidatedTransaction` call.
///
/// Pairs the wire form with its hash because the hash is not recoverable from
/// [`ValidatedTransaction`] without re-decoding `raw`, and every send path needs it for metrics and
/// event attribution.
#[derive(Debug, Clone)]
pub struct InsertValidatedTransaction<E = NoExtensions> {
    /// The transaction in builder-RPC wire form.
    pub transaction: ValidatedTransaction<E>,
    /// Hash of the transaction being forwarded.
    pub tx_hash: TxHash,
}

impl<E> ForwardRequest for InsertValidatedTransaction<E>
where
    E: Serialize + Send + Sync + 'static,
{
    fn method(&self) -> &'static str {
        "base_insertValidatedTransaction"
    }

    fn params(&self) -> Result<ArrayParams, serde_json::Error> {
        let mut params = ArrayParams::new();
        params.insert(&self.transaction)?;
        Ok(params)
    }

    fn tx_hash(&self) -> Option<TxHash> {
        Some(self.tx_hash)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use jsonrpsee::core::traits::ToRpcParams;
    use serde_json::{Value, json};

    use super::*;

    fn insert(tx_hash: TxHash) -> InsertValidatedTransaction {
        InsertValidatedTransaction {
            transaction: ValidatedTransaction {
                sender: Address::repeat_byte(0x11),
                raw: Bytes::from_static(&[0x02, 0x03]),
                extensions: NoExtensions {},
            },
            tx_hash,
        }
    }

    /// The params must reach the wire as the single-element positional array the builder RPC
    /// expects, not as a bare transaction object. A mismatch here fails only at runtime, against a
    /// real peer, so it is asserted on the serialized form rather than on the builder call.
    #[test]
    fn params_serialize_as_a_single_positional_argument() {
        let raw = insert(B256::repeat_byte(0xaa))
            .params()
            .expect("serializable")
            .to_rpc_params()
            .expect("valid params")
            .expect("params present");
        let encoded: Value = serde_json::from_str(raw.get()).expect("valid json");

        assert_eq!(encoded.as_array().expect("positional array").len(), 1);
        assert_eq!(encoded[0]["sender"], json!("0x1111111111111111111111111111111111111111"));
        assert!(encoded[0].get("max_block_number").is_none());
        assert!(encoded[0].get("min_block_number").is_none());
    }

    #[test]
    fn insert_reports_its_method_and_hash() {
        let hash = B256::repeat_byte(0xbb);
        let request = insert(hash);

        assert_eq!(request.method(), "base_insertValidatedTransaction");
        assert_eq!(request.tx_hash(), Some(hash));
    }
}
