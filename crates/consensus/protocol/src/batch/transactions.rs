//! This module contains the [`SpanBatchTransactions`] type and logic for encoding and decoding
//! transactions in a span batch.

use alloc::vec::Vec;

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, U256, bytes};
use alloy_rlp::{Buf, Decodable, Encodable};
use base_common_consensus::{BaseTxEnvelope, OpTxType};

use crate::{
    SpanBatchBits, SpanBatchEip8130TransactionData, SpanBatchElement, SpanBatchError,
    SpanBatchTransactionData, SpanDecodingError, read_tx_data,
};

/// This struct contains the decoded information for transactions in a span batch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpanBatchTransactions {
    /// The total number of transactions in a span batch. Must be manually set.
    pub total_block_tx_count: u64,
    /// The contract creation bits, standard span-batch bitlist.
    pub contract_creation_bits: SpanBatchBits,
    /// The transaction signatures.
    pub tx_sigs: Vec<Signature>,
    /// The transaction nonces
    pub tx_nonces: Vec<u64>,
    /// The transaction gas limits.
    pub tx_gases: Vec<u64>,
    /// The `to` addresses of the transactions.
    pub tx_tos: Vec<Address>,
    /// The transaction data.
    pub tx_data: Vec<Vec<u8>>,
    /// The protected bits, standard span-batch bitlist.
    pub protected_bits: SpanBatchBits,
    /// The types of the transactions.
    pub tx_types: Vec<OpTxType>,
    /// Total legacy transaction count in the span batch.
    pub legacy_tx_count: u64,
    /// The EIP-8130 auth proofs, one `(sender_proof, payer_proof)` pair per
    /// EIP-8130 transaction in transaction order.
    pub eip8130_auth_data: Vec<(Bytes, Bytes)>,
}

impl SpanBatchTransactions {
    /// Encodes the [`SpanBatchTransactions`] into a writer.
    pub fn encode(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        self.encode_contract_creation_bits(w)?;
        self.encode_tx_sigs(w)?;
        self.encode_tx_tos(w)?;
        self.encode_tx_data(w)?;
        self.encode_tx_nonces(w)?;
        self.encode_tx_gases(w)?;
        self.encode_protected_bits(w)?;
        self.encode_eip8130_auth_data(w)?;
        Ok(())
    }

    /// Decodes the [`SpanBatchTransactions`] from a reader.
    pub fn decode(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        self.decode_contract_creation_bits(r)?;
        self.decode_tx_sigs(r)?;
        self.decode_tx_tos(r)?;
        self.decode_tx_data(r)?;
        self.decode_tx_nonces(r)?;
        self.decode_tx_gases(r)?;
        self.decode_protected_bits(r)?;
        self.decode_eip8130_auth_data(r)?;
        Ok(())
    }

    /// Encode the contract creation bits into a writer.
    pub fn encode_contract_creation_bits(
        &self,
        w: &mut dyn bytes::BufMut,
    ) -> Result<(), SpanBatchError> {
        SpanBatchBits::encode(w, self.total_block_tx_count as usize, &self.contract_creation_bits)?;
        Ok(())
    }

    /// Encode the protected bits into a writer.
    pub fn encode_protected_bits(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        SpanBatchBits::encode(w, self.legacy_tx_count as usize, &self.protected_bits)?;
        Ok(())
    }

    /// Encode the transaction signatures into a writer (excluding `v` field).
    pub fn encode_tx_sigs(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        let mut y_parity_bits = SpanBatchBits::default();
        for (i, sig) in self.tx_sigs.iter().enumerate() {
            y_parity_bits.set_bit(i, sig.v());
        }

        SpanBatchBits::encode(w, self.total_block_tx_count as usize, &y_parity_bits)?;
        for sig in &self.tx_sigs {
            w.put_slice(&sig.r().to_be_bytes::<32>());
            w.put_slice(&sig.s().to_be_bytes::<32>());
        }
        Ok(())
    }

    /// Encode the transaction nonces into a writer.
    pub fn encode_tx_nonces(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        let mut buf = [0u8; 10];
        for nonce in &self.tx_nonces {
            let slice = unsigned_varint::encode::u64(*nonce, &mut buf);
            w.put_slice(slice);
        }
        Ok(())
    }

    /// Encode the transaction gas limits into a writer.
    pub fn encode_tx_gases(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        let mut buf = [0u8; 10];
        for gas in &self.tx_gases {
            let slice = unsigned_varint::encode::u64(*gas, &mut buf);
            w.put_slice(slice);
        }
        Ok(())
    }

    /// Encode the `to` addresses of the transactions into a writer.
    pub fn encode_tx_tos(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        for to in &self.tx_tos {
            w.put_slice(to.as_ref());
        }
        Ok(())
    }

    /// Encode the transaction data into a writer.
    pub fn encode_tx_data(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        for data in &self.tx_data {
            w.put_slice(data);
        }
        Ok(())
    }

    /// Encode the EIP-8130 auth proofs into a writer. Each bundle is the
    /// uvarint-prefixed sender proof followed by the uvarint-prefixed payer proof.
    pub fn encode_eip8130_auth_data(
        &self,
        w: &mut dyn bytes::BufMut,
    ) -> Result<(), SpanBatchError> {
        let mut buf = [0u8; 10];
        for (sender_proof, payer_proof) in &self.eip8130_auth_data {
            for proof in [sender_proof, payer_proof] {
                let len = unsigned_varint::encode::u64(proof.len() as u64, &mut buf);
                w.put_slice(len);
                w.put_slice(proof);
            }
        }
        Ok(())
    }

    /// Decode the contract creation bits from a reader.
    pub fn decode_contract_creation_bits(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        if self.total_block_tx_count > SpanBatchElement::MAX_SPAN_BATCH_ELEMENTS {
            return Err(SpanBatchError::TooBigSpanBatchSize);
        }

        self.contract_creation_bits = SpanBatchBits::decode(r, self.total_block_tx_count as usize)?;
        Ok(())
    }

    /// Decode the protected bits from a reader.
    pub fn decode_protected_bits(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        if self.legacy_tx_count > SpanBatchElement::MAX_SPAN_BATCH_ELEMENTS {
            return Err(SpanBatchError::TooBigSpanBatchSize);
        }

        self.protected_bits = SpanBatchBits::decode(r, self.legacy_tx_count as usize)?;
        Ok(())
    }

    /// Decode the transaction signatures from a reader (excluding `v` field).
    pub fn decode_tx_sigs(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        let y_parity_bits = SpanBatchBits::decode(r, self.total_block_tx_count as usize)?;
        let mut sigs = Vec::with_capacity(self.total_block_tx_count as usize);
        for i in 0..self.total_block_tx_count {
            let y_parity = y_parity_bits.get_bit(i as usize).expect("same length");
            if r.len() < 64 {
                return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData));
            }
            let r_val = U256::from_be_slice(&r[..32]);
            let s_val = U256::from_be_slice(&r[32..64]);
            sigs.push(Signature::new(r_val, s_val, y_parity == 1));
            r.advance(64);
        }
        self.tx_sigs = sigs;
        Ok(())
    }

    /// Decode the transaction nonces from a reader.
    pub fn decode_tx_nonces(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        let mut nonces = Vec::with_capacity(self.total_block_tx_count as usize);
        for _ in 0..self.total_block_tx_count {
            let (nonce, remaining) = unsigned_varint::decode::u64(r)
                .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::TxNonces))?;
            nonces.push(nonce);
            *r = remaining;
        }
        self.tx_nonces = nonces;
        Ok(())
    }

    /// Decode the transaction gas limits from a reader.
    pub fn decode_tx_gases(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        let mut gases = Vec::with_capacity(self.total_block_tx_count as usize);
        for _ in 0..self.total_block_tx_count {
            let (gas, remaining) = unsigned_varint::decode::u64(r)
                .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::TxNonces))?;
            gases.push(gas);
            *r = remaining;
        }
        self.tx_gases = gases;
        Ok(())
    }

    /// Decode the `to` addresses of the transactions from a reader.
    pub fn decode_tx_tos(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        let mut tos = Vec::with_capacity(self.total_block_tx_count as usize);
        let contract_creation_count = self.contract_creation_count();
        for _ in 0..(self.total_block_tx_count - contract_creation_count) {
            if r.len() < 20 {
                return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData));
            }
            let to = Address::from_slice(&r[..20]);
            tos.push(to);
            r.advance(20);
        }
        self.tx_tos = tos;
        Ok(())
    }

    /// Decode the transaction data from a reader.
    pub fn decode_tx_data(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        let mut tx_data = Vec::new();
        let mut tx_types = Vec::new();

        // Do not need the transaction data header because the RLP stream already includes the
        // length information.
        for _ in 0..self.total_block_tx_count {
            let (tx_data_item, tx_type) = read_tx_data(r)?;
            tx_data.push(tx_data_item);
            tx_types.push(tx_type);
            if matches!(tx_type, OpTxType::Legacy) {
                self.legacy_tx_count += 1;
            }
        }

        self.tx_data = tx_data;
        self.tx_types = tx_types;

        Ok(())
    }

    /// Decode the EIP-8130 auth proofs from a reader, reading one
    /// `(sender_proof, payer_proof)` bundle for each EIP-8130 transaction in
    /// transaction order.
    pub fn decode_eip8130_auth_data(&mut self, r: &mut &[u8]) -> Result<(), SpanBatchError> {
        let mut auth_data = Vec::new();
        for tx_type in &self.tx_types {
            if !matches!(tx_type, OpTxType::Eip8130) {
                continue;
            }
            let sender_proof = Self::decode_eip8130_proof(r)?;
            let payer_proof = Self::decode_eip8130_proof(r)?;
            auth_data.push((sender_proof, payer_proof));
        }
        self.eip8130_auth_data = auth_data;
        Ok(())
    }

    /// Decode a single uvarint-prefixed EIP-8130 auth proof from a reader.
    fn decode_eip8130_proof(r: &mut &[u8]) -> Result<Bytes, SpanBatchError> {
        let (n, remaining) = unsigned_varint::decode::u64(r)
            .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::InvalidAuthData))?;
        *r = remaining;
        if n > SpanBatchEip8130TransactionData::MAX_AUTH_PROOF_BYTES {
            return Err(SpanBatchError::TooBigSpanBatchSize);
        }
        let n = n as usize;
        if r.len() < n {
            return Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData));
        }
        let proof = Bytes::copy_from_slice(&r[..n]);
        r.advance(n);
        Ok(proof)
    }

    /// Returns the number of contract creation transactions in the span batch.
    pub fn contract_creation_count(&self) -> u64 {
        self.contract_creation_bits.as_ref().iter().map(|b| b.count_ones() as u64).sum()
    }

    /// Retrieve all of the raw transactions from the [`SpanBatchTransactions`].
    pub fn full_txs(&self, chain_id: u64) -> Result<Vec<Vec<u8>>, SpanBatchError> {
        let mut txs = Vec::new();
        let mut to_idx = 0;
        let mut protected_bit_idx = 0;
        let mut eip8130_idx = 0;
        for idx in 0..self.total_block_tx_count {
            let mut data = self.tx_data[idx as usize].as_slice();
            let tx = SpanBatchTransactionData::decode(&mut data)
                .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
            let nonce = self
                .tx_nonces
                .get(idx as usize)
                .ok_or(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
            let gas = self
                .tx_gases
                .get(idx as usize)
                .ok_or(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
            let bit = self
                .contract_creation_bits
                .get_bit(idx as usize)
                .ok_or(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
            let to = if bit == 0 {
                if self.tx_tos.len() <= to_idx {
                    return Err(SpanBatchError::Decoding(
                        SpanDecodingError::InvalidTransactionData,
                    ));
                }
                to_idx += 1;
                Some(self.tx_tos[to_idx - 1])
            } else {
                None
            };

            // EIP-8130 transactions are reassembled from the remainder and the auth proofs
            // carried in the trailing column, then re-encoded directly.
            if let SpanBatchTransactionData::Eip8130(data) = &tx {
                let (sender_proof, payer_proof) = self
                    .eip8130_auth_data
                    .get(eip8130_idx)
                    .ok_or(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
                eip8130_idx += 1;
                let signed =
                    data.to_tx(chain_id, *nonce, *gas, sender_proof.clone(), payer_proof.clone());
                let mut buf = Vec::new();
                BaseTxEnvelope::Eip8130(signed).encode_2718(&mut buf);
                txs.push(buf);
                continue;
            }

            let sig = *self
                .tx_sigs
                .get(idx as usize)
                .ok_or(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
            let is_protected = if tx.tx_type() == OpTxType::Legacy {
                protected_bit_idx += 1;
                self.protected_bits.get_bit(protected_bit_idx - 1).unwrap_or_default() == 1
            } else {
                true
            };
            let tx_envelope = tx.to_signed_tx(*nonce, *gas, to, chain_id, sig, is_protected)?;
            let mut buf = Vec::new();
            tx_envelope.encode_2718(&mut buf);
            txs.push(buf);
        }
        Ok(txs)
    }

    /// Add raw transactions into the [`SpanBatchTransactions`].
    pub fn add_txs(&mut self, txs: Vec<Bytes>, chain_id: u64) -> Result<(), SpanBatchError> {
        let total_block_tx_count = txs.len() as u64;
        let offset = self.total_block_tx_count;

        for i in 0..total_block_tx_count {
            let tx_enveloped = BaseTxEnvelope::decode(&mut txs[i as usize].as_ref())
                .map_err(|_| SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))?;
            let span_batch_tx = SpanBatchTransactionData::try_from(&tx_enveloped)?;
            let tx_type = tx_enveloped.tx_type();

            let (signature, to, nonce, gas) = match &tx_enveloped {
                BaseTxEnvelope::Legacy(tx) => {
                    let (tx, sig) = (tx.tx(), tx.signature());
                    let protected = tx.chain_id.is_some();
                    if protected && tx.chain_id != Some(chain_id) {
                        return Err(SpanBatchError::Decoding(
                            SpanDecodingError::InvalidTransactionData,
                        ));
                    }
                    self.protected_bits.set_bit(self.legacy_tx_count as usize, protected);
                    self.legacy_tx_count += 1;
                    (*sig, tx.to(), tx.nonce(), tx.gas_limit())
                }
                BaseTxEnvelope::Eip2930(tx) => {
                    let (tx, sig) = (tx.tx(), tx.signature());
                    if tx.chain_id != chain_id {
                        return Err(SpanBatchError::Decoding(
                            SpanDecodingError::InvalidTransactionData,
                        ));
                    }
                    (*sig, tx.to(), tx.nonce(), tx.gas_limit())
                }
                BaseTxEnvelope::Eip1559(tx) => {
                    let (tx, sig) = (tx.tx(), tx.signature());
                    if tx.chain_id != chain_id {
                        return Err(SpanBatchError::Decoding(
                            SpanDecodingError::InvalidTransactionData,
                        ));
                    }
                    (*sig, tx.to(), tx.nonce(), tx.gas_limit())
                }
                BaseTxEnvelope::Eip7702(tx) => {
                    let (tx, sig) = (tx.tx(), tx.signature());
                    if tx.chain_id != chain_id {
                        return Err(SpanBatchError::Decoding(
                            SpanDecodingError::InvalidTransactionData,
                        ));
                    }
                    (*sig, tx.to(), tx.nonce(), tx.gas_limit())
                }
                BaseTxEnvelope::Eip8130(signed) => {
                    let inner = signed.tx();
                    if inner.chain_id != chain_id {
                        return Err(SpanBatchError::Decoding(
                            SpanDecodingError::InvalidTransactionData,
                        ));
                    }
                    let sender_proof = SpanBatchEip8130TransactionData::split_auth(
                        signed.sender_auth(),
                        inner.sender.is_some(),
                    )?
                    .1;
                    let payer_proof = SpanBatchEip8130TransactionData::split_auth(
                        signed.payer_auth(),
                        inner.payer.is_some(),
                    )?
                    .1;
                    self.eip8130_auth_data.push((sender_proof, payer_proof));
                    (
                        Signature::new(U256::ZERO, U256::ZERO, false),
                        None,
                        inner.nonce_sequence,
                        inner.gas_limit,
                    )
                }
                BaseTxEnvelope::Deposit(_) => {
                    return Err(SpanBatchError::Decoding(
                        SpanDecodingError::InvalidTransactionType,
                    ));
                }
            };

            let contract_creation_bit = match to {
                Some(address) => {
                    self.tx_tos.push(address);
                    0
                }
                None => 1,
            };
            let mut tx_data_buf = Vec::new();
            span_batch_tx.encode(&mut tx_data_buf);

            self.tx_sigs.push(signature);
            self.contract_creation_bits.set_bit((i + offset) as usize, contract_creation_bit == 1);
            self.tx_nonces.push(nonce);
            self.tx_data.push(tx_data_buf);
            self.tx_gases.push(gas);
            self.tx_types.push(tx_type);
        }
        self.total_block_tx_count += total_block_tx_count;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::{Signed, TxEip1559, TxEip2930, TxEip7702, TxEnvelope, TxLegacy};
    use alloy_primitives::{B256, Signature, TxKind, address};
    use base_common_consensus::{
        AccountChange, ActorChange, ActorChangeType, Call, ConfigChange, CreateEntry, Delegation,
        Eip8130Signed, InitialActor, TxEip8130,
    };

    use super::*;

    const EIP8130_CHAIN_ID: u64 = 8453;

    /// Regression: truncated input to `decode_tx_sigs` must return an error, not panic.
    /// A dishonest batcher can craft a span batch with fewer bytes than the declared tx count
    /// requires, which previously caused an out-of-bounds slice panic.
    #[test]
    fn test_decode_tx_sigs_truncated_input() {
        let mut txs = SpanBatchTransactions { total_block_tx_count: 1, ..Default::default() };
        // y_parity bitfield for 1 tx = 1 byte (all zeros = false parity), then we need 64 bytes
        // for r+s. Provide only 32 bytes to trigger the bounds check.
        let truncated = [0u8; 33]; // 1 byte bitfield + 32 bytes (not enough for 64-byte sig)
        assert_eq!(
            txs.decode_tx_sigs(&mut truncated.as_ref()),
            Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))
        );
    }

    /// Regression: truncated input to `decode_tx_tos` must return an error, not panic.
    /// A dishonest batcher can craft a span batch with fewer bytes than the declared non-contract
    /// tx count requires, which previously caused an out-of-bounds slice panic.
    #[test]
    fn test_decode_tx_tos_truncated_input() {
        let mut txs = SpanBatchTransactions { total_block_tx_count: 1, ..Default::default() };
        // contract_creation_bits is all zeros (default), so contract_creation_count = 0,
        // meaning we expect 1 `to` address (20 bytes). Provide only 19 bytes.
        let truncated = [0u8; 19];
        assert_eq!(
            txs.decode_tx_tos(&mut truncated.as_ref()),
            Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData))
        );
    }

    /// Regression: a length-prefix uvarint that never terminates must error as
    /// auth data, not as nonce data.
    #[test]
    fn test_decode_eip8130_proof_truncated_length_prefix() {
        let mut buf: &[u8] = &[0xff, 0xff, 0xff];
        assert_eq!(
            SpanBatchTransactions::decode_eip8130_proof(&mut buf),
            Err(SpanBatchError::Decoding(SpanDecodingError::InvalidAuthData))
        );
    }

    /// Regression: a declared proof length above the byte cap fails fast before
    /// any copy is attempted.
    #[test]
    fn test_decode_eip8130_proof_rejects_oversized_length() {
        let oversized = SpanBatchEip8130TransactionData::MAX_AUTH_PROOF_BYTES + 1;
        let mut varint_buf = unsigned_varint::encode::u64_buffer();
        let prefix = unsigned_varint::encode::u64(oversized, &mut varint_buf);
        let mut buf: &[u8] = prefix;
        assert_eq!(
            SpanBatchTransactions::decode_eip8130_proof(&mut buf),
            Err(SpanBatchError::TooBigSpanBatchSize)
        );
    }

    #[test]
    fn test_span_batch_transactions_add_empty_txs() {
        let mut span_batch_txs = SpanBatchTransactions::default();
        let txs = vec![];
        let chain_id = 1;
        let result = span_batch_txs.add_txs(txs, chain_id);
        assert!(result.is_ok());
        assert_eq!(span_batch_txs.total_block_tx_count, 0);
    }

    #[test]
    fn test_span_batch_transactions_add_eip2930_tx_wrong_chain_id() {
        let sig = Signature::test_signature();
        let to = address!("0123456789012345678901234567890123456789");
        let tx = TxEnvelope::Eip2930(Signed::new_unchecked(
            TxEip2930 { to: TxKind::Call(to), ..Default::default() },
            sig,
            Default::default(),
        ));
        let mut span_batch_txs = SpanBatchTransactions::default();
        let mut buf = vec![];
        tx.encode(&mut buf);
        let txs = vec![Bytes::from(buf)];
        let chain_id = 1;
        let err = span_batch_txs.add_txs(txs, chain_id).unwrap_err();
        assert_eq!(err, SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData));
    }

    #[rstest::rstest]
    #[case::eip2930(TxEnvelope::Eip2930(Signed::new_unchecked(
        TxEip2930 { to: TxKind::Call(address!("0123456789012345678901234567890123456789")), chain_id: 1, ..Default::default() },
        Signature::test_signature(), Default::default(),
    )))]
    #[case::eip1559(TxEnvelope::Eip1559(Signed::new_unchecked(
        TxEip1559 { to: TxKind::Call(address!("0123456789012345678901234567890123456789")), chain_id: 1, ..Default::default() },
        Signature::test_signature(), Default::default(),
    )))]
    #[case::eip7702(TxEnvelope::Eip7702(Signed::new_unchecked(
        TxEip7702 { to: address!("0123456789012345678901234567890123456789"), chain_id: 1, ..Default::default() },
        Signature::test_signature(), Default::default(),
    )))]
    fn test_span_batch_transactions_add_tx(#[case] tx: TxEnvelope) {
        let mut span_batch_txs = SpanBatchTransactions::default();
        let mut buf = vec![];
        tx.encode(&mut buf);
        let result = span_batch_txs.add_txs(vec![Bytes::from(buf)], 1);
        assert_eq!(result, Ok(()));
        assert_eq!(span_batch_txs.total_block_tx_count, 1);
    }

    /// Encodes an EIP-8130 transaction to its raw EIP-2718 form for round-trip inputs.
    fn eip8130_raw(tx: TxEip8130, sender_auth: Bytes, payer_auth: Bytes) -> Bytes {
        let mut buf = Vec::new();
        BaseTxEnvelope::Eip8130(Eip8130Signed::new(tx, sender_auth, payer_auth))
            .encode_2718(&mut buf);
        Bytes::from(buf)
    }

    /// EOA self-pay EIP-8130 body; variants clone it and override individual fields.
    fn eip8130_body() -> TxEip8130 {
        TxEip8130 {
            chain_id: EIP8130_CHAIN_ID,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 0,
            gas_limit: 21_000,
            account_changes: vec![],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        }
    }

    /// Drives one `add_txs -> encode -> decode -> full_txs` cycle and asserts the
    /// reconstructed raw transactions are identical to the inputs.
    fn assert_span_batch_roundtrip(raws: Vec<Bytes>, chain_id: u64) {
        let count = raws.len() as u64;
        let mut batch = SpanBatchTransactions::default();
        batch.add_txs(raws.clone(), chain_id).unwrap();
        let mut buf = Vec::new();
        batch.encode(&mut buf).unwrap();

        let mut decoded =
            SpanBatchTransactions { total_block_tx_count: count, ..Default::default() };
        decoded.decode(&mut buf.as_slice()).unwrap();
        let rebuilt: Vec<Bytes> =
            decoded.full_txs(chain_id).unwrap().into_iter().map(Bytes::from).collect();

        assert_eq!(rebuilt, raws);
    }

    #[test]
    fn test_eip8130_span_batch_roundtrip() {
        let sender = address!("00000000000000000000000000000000000000aa");
        let payer = address!("00000000000000000000000000000000000000bb");

        // EOA self-pay: no explicit sender/payer, so the whole sender_auth is the proof.
        let eoa = {
            let mut tx = eip8130_body();
            tx.nonce_sequence = 7;
            tx.max_priority_fee_per_gas = 1_000_000_000;
            tx.max_fee_per_gas = 5_000_000_000;
            tx.calls = vec![vec![Call {
                to: address!("00000000000000000000000000000000000000dd"),
                data: bytes!("deadbeef"),
            }]];
            eip8130_raw(tx, Bytes::from_static(&[0xab; 65]), Bytes::new())
        };

        // Configured sender and explicit payer: each auth is a 20-byte authenticator
        // address followed by a proof, which the codec splits across the remainder and
        // the trailing auth column.
        let configured = {
            let mut tx = eip8130_body();
            tx.sender = Some(sender);
            tx.payer = Some(payer);
            tx.nonce_key = U256::from(0x1234u64);
            tx.nonce_sequence = 3;
            tx.expiry = 1_900_000_000;
            let mut sender_auth = sender.as_slice().to_vec();
            sender_auth.extend_from_slice(&[0xcd; 32]);
            let mut payer_auth = payer.as_slice().to_vec();
            payer_auth.extend_from_slice(&[0xef; 16]);
            eip8130_raw(tx, Bytes::from(sender_auth), Bytes::from(payer_auth))
        };

        // Every account-change variant plus multi-phase calls and metadata.
        let rich = {
            let mut tx = eip8130_body();
            tx.sender = Some(sender);
            tx.nonce_sequence = 11;
            tx.account_changes = vec![
                AccountChange::Create(CreateEntry {
                    user_salt: B256::repeat_byte(0x01),
                    code: bytes!("60006000fd"),
                    initial_actors: vec![InitialActor {
                        actor_id: B256::repeat_byte(0x02),
                        authenticator: address!("00000000000000000000000000000000000000cc"),
                    }],
                }),
                AccountChange::ConfigChange(ConfigChange {
                    chain_id: EIP8130_CHAIN_ID,
                    sequence: 1,
                    actor_changes: vec![
                        ActorChange {
                            change_type: ActorChangeType::Authorize,
                            actor_id: B256::repeat_byte(0x03),
                            data: bytes!("aabbcc"),
                        },
                        ActorChange {
                            change_type: ActorChangeType::Revoke,
                            actor_id: B256::repeat_byte(0x04),
                            data: Bytes::new(),
                        },
                    ],
                    auth: bytes!("c0ffee"),
                }),
                AccountChange::Delegation(Delegation {
                    target: address!("00000000000000000000000000000000000000ee"),
                }),
            ];
            tx.calls = vec![
                vec![
                    Call {
                        to: address!("0000000000000000000000000000000000000001"),
                        data: bytes!("11"),
                    },
                    Call {
                        to: address!("0000000000000000000000000000000000000002"),
                        data: bytes!("2222"),
                    },
                ],
                vec![Call { to: Address::ZERO, data: Bytes::new() }],
            ];
            tx.metadata = bytes!("decafbad");
            let mut sender_auth = sender.as_slice().to_vec();
            sender_auth.extend_from_slice(&[0x99; 48]);
            eip8130_raw(tx, Bytes::from(sender_auth), Bytes::new())
        };

        // Minimal body: empty auth, no account changes or calls.
        let minimal = eip8130_raw(eip8130_body(), Bytes::new(), Bytes::new());

        assert_span_batch_roundtrip(vec![eoa, configured, rich, minimal], EIP8130_CHAIN_ID);
    }

    #[test]
    fn test_span_batch_mixed_eip8130_roundtrip() {
        let to = address!("0123456789012345678901234567890123456789");
        let sig = Signature::test_signature();

        let mut legacy = Vec::new();
        TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(EIP8130_CHAIN_ID),
                nonce: 1,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(to),
                value: U256::from(1u64),
                input: bytes!("00"),
            },
            sig,
            Default::default(),
        ))
        .encode_2718(&mut legacy);

        let mut eip1559 = Vec::new();
        TxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: EIP8130_CHAIN_ID,
                nonce: 2,
                gas_limit: 50_000,
                max_fee_per_gas: 5_000_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
                to: TxKind::Call(to),
                ..Default::default()
            },
            sig,
            Default::default(),
        ))
        .encode_2718(&mut eip1559);

        let mut eip7702 = Vec::new();
        TxEnvelope::Eip7702(Signed::new_unchecked(
            TxEip7702 {
                to,
                chain_id: EIP8130_CHAIN_ID,
                nonce: 3,
                gas_limit: 60_000,
                ..Default::default()
            },
            sig,
            Default::default(),
        ))
        .encode_2718(&mut eip7702);

        let aa_eoa = {
            let mut tx = eip8130_body();
            tx.nonce_sequence = 4;
            eip8130_raw(tx, Bytes::from_static(&[0x01; 65]), Bytes::new())
        };
        let aa_configured = {
            let mut tx = eip8130_body();
            tx.sender = Some(to);
            tx.nonce_sequence = 5;
            let mut sender_auth = to.as_slice().to_vec();
            sender_auth.extend_from_slice(&[0x02; 16]);
            eip8130_raw(tx, Bytes::from(sender_auth), Bytes::new())
        };

        assert_span_batch_roundtrip(
            vec![
                Bytes::from(legacy),
                aa_eoa,
                Bytes::from(eip1559),
                aa_configured,
                Bytes::from(eip7702),
            ],
            EIP8130_CHAIN_ID,
        );
    }
}
