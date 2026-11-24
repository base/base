//! Block headers and bodies as they travel on the eth wire, and the checks that tie a body to
//! its header.

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, Header};
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use alloy_trie::root::ordered_trie_root_with_encoder;

/// A header with its hash (keccak of its RLP encoding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedHeader {
    /// The block hash.
    pub hash: B256,
    /// The header.
    pub header: Header,
}

impl SealedHeader {
    /// Seals a header by hashing its encoding.
    pub fn seal(header: Header) -> Self {
        Self { hash: header.hash_slow(), header }
    }

    /// Decodes one header from `buf`, hashing the exact bytes received.
    pub fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let start = *buf;
        let header = Header::decode(buf)?;
        let consumed = start.len() - buf.len();
        Ok(Self { hash: keccak256(&start[..consumed]), header })
    }

    /// The block number.
    pub const fn number(&self) -> u64 {
        self.header.number
    }
}

/// A block body: the transactions in their EIP-2718 encoding, the ommers (always empty on Base),
/// and the withdrawals (present and empty on Base from Canyon on).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockBody {
    /// Each transaction's EIP-2718 encoding: the RLP list for legacy, `type || payload` otherwise.
    pub transactions: Vec<Bytes>,
    /// Ommer headers.
    pub ommers: Vec<Header>,
    /// Withdrawals, `None` before Shanghai/Canyon.
    pub withdrawals: Option<Vec<Withdrawal>>,
}

/// Why a body does not belong to a header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// The transactions root differs.
    TransactionsRoot,
    /// The ommers hash differs.
    OmmersHash,
    /// Withdrawals present without a header root, missing with one, or not empty (Base never
    /// has withdrawals; from Isthmus the header root is the message passer storage root).
    Withdrawals,
}

impl BlockBody {
    /// Checks that the body matches the header's commitments.
    pub fn validate(&self, header: &Header) -> Result<(), BodyError> {
        if self.transactions_root() != header.transactions_root {
            return Err(BodyError::TransactionsRoot);
        }
        let ommers_hash = if self.ommers.is_empty() {
            EMPTY_OMMER_ROOT_HASH
        } else {
            keccak256(alloy_rlp::encode(&self.ommers))
        };
        if ommers_hash != header.ommers_hash {
            return Err(BodyError::OmmersHash);
        }
        match (&self.withdrawals, header.withdrawals_root) {
            (None, None) => {}
            (Some(withdrawals), Some(_)) if withdrawals.is_empty() => {}
            _ => return Err(BodyError::Withdrawals),
        }
        Ok(())
    }

    /// The transactions trie root over the EIP-2718 encodings.
    pub fn transactions_root(&self) -> B256 {
        ordered_trie_root_with_encoder(&self.transactions, |tx, out| out.extend_from_slice(tx))
    }

    fn tx_wire_length(tx: &Bytes) -> usize {
        if tx.first().is_some_and(|b| *b >= 0xc0) { tx.len() } else { tx.as_ref().length() }
    }

    fn payload_length(&self) -> usize {
        let txs: usize = self.transactions.iter().map(Self::tx_wire_length).sum();
        let mut length = RlpHeader { list: true, payload_length: txs }.length_with_payload();
        length += self.ommers.length();
        if let Some(withdrawals) = &self.withdrawals {
            length += withdrawals.length();
        }
        length
    }
}

impl Encodable for BlockBody {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        RlpHeader { list: true, payload_length: self.payload_length() }.encode(out);
        let txs: usize = self.transactions.iter().map(Self::tx_wire_length).sum();
        RlpHeader { list: true, payload_length: txs }.encode(out);
        for tx in &self.transactions {
            // Legacy transactions are inlined as lists; typed ones are wrapped in a string.
            if tx.first().is_some_and(|b| *b >= 0xc0) {
                out.put_slice(tx);
            } else {
                tx.as_ref().encode(out);
            }
        }
        self.ommers.encode(out);
        if let Some(withdrawals) = &self.withdrawals {
            withdrawals.encode(out);
        }
    }

    fn length(&self) -> usize {
        RlpHeader { list: true, payload_length: self.payload_length() }.length_with_payload()
    }
}

impl Decodable for BlockBody {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = RlpHeader::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let (mut items, rest) =
            buf.split_at_checked(header.payload_length).ok_or(alloy_rlp::Error::InputTooShort)?;
        *buf = rest;
        let txs_header = RlpHeader::decode(&mut items)?;
        if !txs_header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let (mut txs, after) = items
            .split_at_checked(txs_header.payload_length)
            .ok_or(alloy_rlp::Error::InputTooShort)?;
        items = after;
        let mut transactions = Vec::new();
        while !txs.is_empty() {
            let start = txs;
            let tx_header = RlpHeader::decode(&mut txs)?;
            if txs.len() < tx_header.payload_length {
                return Err(alloy_rlp::Error::InputTooShort);
            }
            if tx_header.list {
                let consumed = start.len() - txs.len() + tx_header.payload_length;
                transactions.push(Bytes::copy_from_slice(&start[..consumed]));
            } else {
                if tx_header.payload_length == 0 {
                    return Err(alloy_rlp::Error::InputTooShort);
                }
                transactions.push(Bytes::copy_from_slice(&txs[..tx_header.payload_length]));
            }
            txs = &txs[tx_header.payload_length..];
        }
        let ommers = Vec::<Header>::decode(&mut items)?;
        let withdrawals =
            if items.is_empty() { None } else { Some(Vec::<Withdrawal>::decode(&mut items)?) };
        // Later fields (block access lists and the like) are not used on Base; ignore them.
        Ok(Self { transactions, ommers, withdrawals })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    #[test]
    fn body_round_trips_with_legacy_and_typed_transactions() {
        // A legacy transaction (an RLP list) and a typed one (type byte then a list).
        let legacy = Bytes::from(hex::decode("c50102030405").unwrap());
        let typed = Bytes::from(hex::decode("7ec50102030405").unwrap());
        let body = BlockBody {
            transactions: vec![legacy, typed],
            ommers: vec![],
            withdrawals: Some(vec![]),
        };
        let encoded = alloy_rlp::encode(&body);
        assert_eq!(encoded.len(), body.length());
        assert_eq!(BlockBody::decode(&mut encoded.as_slice()).unwrap(), body);

        let header = Header {
            transactions_root: body.transactions_root(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            withdrawals_root: Some(B256::repeat_byte(7)),
            ..Default::default()
        };
        assert_eq!(body.validate(&header), Ok(()));
        let mut tampered = body.clone();
        tampered.transactions.swap(0, 1);
        assert_eq!(tampered.validate(&header), Err(BodyError::TransactionsRoot));
        let mut with_withdrawal = body;
        with_withdrawal.withdrawals = Some(vec![Withdrawal::default()]);
        assert_eq!(with_withdrawal.validate(&header), Err(BodyError::Withdrawals));
    }
}
