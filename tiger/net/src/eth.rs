//! The `eth` wire protocol, versions 68 and 69, as base-reth speaks it.
//!
//! Spec: <https://github.com/ethereum/devp2p/blob/master/caps/eth.md>. base-reth offers eth/66
//! through eth/71; tiger offers 68 and 69 and the highest common version wins (69 with
//! base-reth). Message ids below are relative to the capability's offset.

use alloy_primitives::{B256, U256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader, PayloadView};

use crate::{forkid::ForkId, rlpx::LocalCapability};

pub(crate) use id::*;

/// The eth versions tiger offers, with how many message ids each uses.
pub const ETH_CAPABILITIES: [LocalCapability; 2] = [
    LocalCapability { name: "eth", version: 68, count: 17 },
    LocalCapability { name: "eth", version: 69, count: 18 },
];

/// Relative eth message ids.
mod id {
    /// Status handshake.
    pub(crate) const STATUS: u8 = 0x00;
    /// Block hash announcement (ignored after the merge).
    pub(crate) const NEW_BLOCK_HASHES: u8 = 0x01;
    /// Full transactions broadcast.
    pub(crate) const TRANSACTIONS: u8 = 0x02;
    /// Header request.
    pub(crate) const GET_BLOCK_HEADERS: u8 = 0x03;
    /// Header response.
    pub(crate) const BLOCK_HEADERS: u8 = 0x04;
    /// Body request.
    pub(crate) const GET_BLOCK_BODIES: u8 = 0x05;
    /// Body response.
    pub(crate) const BLOCK_BODIES: u8 = 0x06;
    /// Full block announcement (not used after the merge).
    pub(crate) const NEW_BLOCK: u8 = 0x07;
    /// Transaction hash announcement.
    pub(crate) const NEW_POOLED_TRANSACTION_HASHES: u8 = 0x08;
    /// Pooled transaction request.
    pub(crate) const GET_POOLED_TRANSACTIONS: u8 = 0x09;
    /// Pooled transaction response.
    pub(crate) const POOLED_TRANSACTIONS: u8 = 0x0a;
    /// Receipt request.
    pub(crate) const GET_RECEIPTS: u8 = 0x0f;
    /// Receipt response.
    pub(crate) const RECEIPTS: u8 = 0x10;
    /// Served block range update (eth/69).
    pub(crate) const BLOCK_RANGE_UPDATE: u8 = 0x11;
}

/// Most headers served per request (reth's `MAX_HEADERS_SERVE`).
pub(crate) const MAX_HEADERS_SERVE: usize = 1024;
/// Most bodies or receipt lists served per request.
pub(crate) const MAX_BODIES_SERVE: usize = 1024;
/// Responses stop growing once they pass this many bytes (reth's `SOFT_RESPONSE_LIMIT`).
pub(crate) const SOFT_RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

/// The Status handshake message, either version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The eth version (68 or 69).
    pub version: u64,
    /// The network id, which is the chain id on Base.
    pub network_id: u64,
    /// The genesis block hash.
    pub genesis: B256,
    /// The sender's fork id at its head.
    pub fork_id: ForkId,
    /// The head block hash.
    pub head_hash: B256,
    /// eth/69: the earliest block the sender serves.
    pub earliest: u64,
    /// eth/69: the head block number.
    pub latest: u64,
}

impl Status {
    /// Encodes in the format of `self.version`.
    pub fn encode_payload(&self) -> Vec<u8> {
        if self.version >= 69 {
            encode_list(&[
                &self.version,
                &self.network_id,
                &self.genesis,
                &self.fork_id,
                &self.earliest,
                &self.latest,
                &self.head_hash,
            ])
        } else {
            // eth/68 carries total difficulty; base-reth does not check it, Base's is zero.
            encode_list(&[
                &self.version,
                &self.network_id,
                &U256::ZERO,
                &self.head_hash,
                &self.genesis,
                &self.fork_id,
            ])
        }
    }

    /// Decodes a Status of the negotiated `version`.
    pub fn decode_payload(version: u64, payload: &[u8]) -> alloy_rlp::Result<Self> {
        let mut items = RlpHeader::decode_bytes(&mut &payload[..], true)?;
        let got = u64::decode(&mut items)?;
        if got != version {
            return Err(alloy_rlp::Error::Custom("status version differs from negotiated"));
        }
        let network_id = u64::decode(&mut items)?;
        let status = if version >= 69 {
            Self {
                version,
                network_id,
                genesis: B256::decode(&mut items)?,
                fork_id: ForkId::decode(&mut items)?,
                earliest: u64::decode(&mut items)?,
                latest: u64::decode(&mut items)?,
                head_hash: B256::decode(&mut items)?,
            }
        } else {
            // base-reth rejects a total difficulty wider than 160 bits.
            if U256::decode(&mut items)?.bit_len() > 160 {
                return Err(alloy_rlp::Error::Custom("total difficulty above 160 bits"));
            }
            let head_hash = B256::decode(&mut items)?;
            let genesis = B256::decode(&mut items)?;
            let fork_id = ForkId::decode(&mut items)?;
            Self { version, network_id, genesis, fork_id, head_hash, earliest: 0, latest: 0 }
        };
        // Like base-reth's derived decoder, extra fields are an error.
        if !items.is_empty() {
            return Err(alloy_rlp::Error::ListLengthMismatch { expected: 0, got: items.len() });
        }
        Ok(status)
    }
}

/// The start of a header request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockId {
    /// By block hash.
    Hash(B256),
    /// By block number.
    Number(u64),
}

/// A GetBlockHeaders request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadersRequest {
    /// The first header.
    pub start: BlockId,
    /// How many headers at most.
    pub limit: u64,
    /// Blocks skipped between consecutive headers.
    pub skip: u64,
    /// Walk towards genesis instead of towards the head.
    pub reverse: bool,
}

impl HeadersRequest {
    /// Encodes `[request_id, [start, limit, skip, reverse]]`.
    pub fn encode_payload(&self, request_id: u64) -> Vec<u8> {
        let start: &dyn Encodable = match &self.start {
            BlockId::Hash(hash) => hash,
            BlockId::Number(number) => number,
        };
        let inner = encode_list(&[start, &self.limit, &self.skip, &self.reverse]);
        encode_list(&[&request_id, &RawRlp(&inner)])
    }

    /// Decodes the request body after the request id.
    pub fn decode(items: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let mut fields = RlpHeader::decode_bytes(items, true)?;
        let first = RlpHeader::decode(&mut { fields })?;
        let start = if !first.list && first.payload_length == 32 {
            BlockId::Hash(B256::decode(&mut fields)?)
        } else {
            BlockId::Number(u64::decode(&mut fields)?)
        };
        Ok(Self {
            start,
            limit: u64::decode(&mut fields)?,
            skip: u64::decode(&mut fields)?,
            reverse: bool::decode(&mut fields)?,
        })
    }
}

/// Wraps already-encoded RLP so it can sit in an [`Encodable`] list.
#[derive(Debug, Clone, Copy)]
pub struct RawRlp<'a>(pub &'a [u8]);

impl Encodable for RawRlp<'_> {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        out.put_slice(self.0);
    }

    fn length(&self) -> usize {
        self.0.len()
    }
}

/// Encodes `items` as an RLP list.
pub(crate) fn encode_list(items: &[&dyn Encodable]) -> Vec<u8> {
    let mut out = Vec::new();
    alloy_rlp::encode_list::<_, dyn Encodable>(items, &mut out);
    out
}

/// Splits a request or response payload `[request_id, body]` into the id and the body bytes.
pub fn split_request_id(payload: &[u8]) -> alloy_rlp::Result<(u64, &[u8])> {
    let mut buf = payload;
    let mut items = RlpHeader::decode_bytes(&mut buf, true)?;
    let id = u64::decode(&mut items)?;
    Ok((id, items))
}

/// Encodes `[request_id, [items...]]` where each item is already-encoded RLP.
pub(crate) fn encode_response<I: AsRef<[u8]>>(request_id: u64, items: &[I]) -> Vec<u8> {
    let inner_length: usize = items.iter().map(|i| i.as_ref().len()).sum();
    let inner_header = RlpHeader { list: true, payload_length: inner_length };
    let payload_length = request_id.length() + inner_header.length_with_payload();
    let mut out = Vec::with_capacity(payload_length + 8);
    RlpHeader { list: true, payload_length }.encode(&mut out);
    request_id.encode(&mut out);
    inner_header.encode(&mut out);
    for item in items {
        out.extend_from_slice(item.as_ref());
    }
    out
}

/// Encodes already-encoded RLP items as one list.
pub(crate) fn encode_list_of_raw<I: AsRef<[u8]>>(items: &[I]) -> Vec<u8> {
    let payload_length: usize = items.iter().map(|i| i.as_ref().len()).sum();
    let mut out = Vec::with_capacity(payload_length + 4);
    RlpHeader { list: true, payload_length }.encode(&mut out);
    for item in items {
        out.extend_from_slice(item.as_ref());
    }
    out
}

/// Converts an EIP-2718 receipt (`type || rlp([status, gas, bloom, logs, ...])`, or the bare list
/// for legacy) into its eth/69 wire form `[type, status, gas, logs, ...]`, which drops the bloom.
/// Base deposit receipts keep their trailing deposit nonce and version fields.
pub(crate) fn receipt_to_eth69(receipt: &[u8]) -> alloy_rlp::Result<Vec<u8>> {
    let (tx_type, mut rest) = match receipt.first() {
        Some(b) if *b >= 0xc0 => (0u8, receipt),
        Some(b) => (*b, &receipt[1..]),
        None => return Err(alloy_rlp::Error::InputTooShort),
    };
    let PayloadView::List(mut items) = RlpHeader::decode_raw(&mut rest)? else {
        return Err(alloy_rlp::Error::UnexpectedString);
    };
    if items.len() < 4 {
        return Err(alloy_rlp::Error::Custom("receipt has too few fields"));
    }
    items.remove(2);
    let payload_length = tx_type.length() + items.iter().map(|i| i.len()).sum::<usize>();
    let mut out = Vec::with_capacity(payload_length + 4);
    RlpHeader { list: true, payload_length }.encode(&mut out);
    tx_type.encode(&mut out);
    for item in items {
        out.extend_from_slice(item);
    }
    Ok(out)
}

/// Encodes an EIP-2718 receipt for an eth/68 Receipts response: legacy as the bare list,
/// typed receipts wrapped in an RLP string.
pub(crate) fn receipt_to_eth68(receipt: &[u8]) -> Vec<u8> {
    if receipt.first().is_some_and(|b| *b >= 0xc0) {
        receipt.to_vec()
    } else {
        alloy_rlp::encode(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    #[test]
    fn status_round_trips_both_versions() {
        for version in [68, 69] {
            let status = Status {
                version,
                network_id: 8453,
                genesis: B256::repeat_byte(1),
                fork_id: ForkId { hash: [1, 2, 3, 4], next: 99 },
                head_hash: B256::repeat_byte(2),
                earliest: if version == 69 { 5 } else { 0 },
                latest: if version == 69 { 10 } else { 0 },
            };
            let encoded = status.encode_payload();
            assert_eq!(Status::decode_payload(version, &encoded).unwrap(), status);
        }
    }

    #[test]
    fn headers_request_matches_spec_example() {
        // From the eth spec style: GetBlockHeaders by number 9999, 5 headers, skip 5, reverse.
        let request =
            HeadersRequest { start: BlockId::Number(9999), limit: 5, skip: 5, reverse: true };
        let encoded = request.encode_payload(1111);
        assert_eq!(hex::encode(&encoded), "ca820457c682270f050501");
        let (id, mut body) = split_request_id(&encoded).unwrap();
        assert_eq!(id, 1111);
        assert_eq!(HeadersRequest::decode(&mut body).unwrap(), request);
        let by_hash = HeadersRequest { start: BlockId::Hash(B256::repeat_byte(9)), ..request };
        let encoded = by_hash.encode_payload(1);
        let (_, mut body) = split_request_id(&encoded).unwrap();
        assert_eq!(HeadersRequest::decode(&mut body).unwrap(), by_hash);
    }

    #[test]
    fn eth69_receipts_drop_the_bloom() {
        // A typed receipt: 0x02 || [status=1, gas=0x5208, bloom (256 zero bytes), logs=[]].
        let mut fields = Vec::new();
        1u8.encode(&mut fields);
        0x5208u64.encode(&mut fields);
        [0u8; 256].encode(&mut fields);
        fields.push(0xc0);
        let mut receipt = vec![0x02];
        RlpHeader { list: true, payload_length: fields.len() }.encode(&mut receipt);
        receipt.extend_from_slice(&fields);
        assert_eq!(hex::encode(receipt_to_eth69(&receipt).unwrap()), "c60201825208c0");
        // Typed receipts are strings in eth/68.
        assert_eq!(receipt_to_eth68(&receipt)[0], 0xb9);
    }
}
