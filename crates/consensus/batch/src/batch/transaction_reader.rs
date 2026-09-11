//! Decoding of transaction payloads in span batches.
use crate::{SpanBatchElement, SpanBatchError, SpanDecodingError};
use alloc::vec::Vec;
use alloy_rlp::{Buf, Header};
use base_common_types_chain::OpTxType;
/// Decoder for compressed span transaction payloads.
#[derive(Debug)]
pub struct SpanTransactionReader;
impl SpanTransactionReader {
    /// Reads the next transaction payload and its type.
    pub fn read(r: &mut &[u8]) -> Result<(Vec<u8>, OpTxType), SpanBatchError> {
        let mut tx_data = Vec::new();
        let first_byte = *r
            .first()
            .ok_or(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
        let mut tx_type = 0;
        if first_byte <= 0x7F {
            // EIP-2718: Non-legacy tx, so write tx type
            tx_type = first_byte;
            tx_data.push(tx_type);
            r.advance(1);
        }

        // Read the RLP header with a different reader pointer. This prevents the initial pointer from
        // being advanced in the case that what we read is invalid.
        let rlp_header = Header::decode(&mut (**r).as_ref())
            .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;

        let tx_payload = if rlp_header.list {
            // Grab the raw RLP for the transaction data from `r`. It was unaffected since we copied it.
            let payload_length_with_header = rlp_header.payload_length + rlp_header.length();
            if payload_length_with_header > SpanBatchElement::MAX_SPAN_BATCH_ELEMENTS as usize {
                return Err(SpanBatchError::TooBigSpanBatchSize);
            }
            if payload_length_with_header > r.len() {
                return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData));
            }
            let payload = r[0..payload_length_with_header].to_vec();
            r.advance(payload_length_with_header);
            Ok(payload)
        } else {
            Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))
        }?;
        tx_data.extend_from_slice(&tx_payload);

        Ok((
            tx_data,
            tx_type
                .try_into()
                .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionType))?,
        ))
    }
}
