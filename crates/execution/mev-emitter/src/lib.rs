//! `base-mev-emitter` — wire-contract types for the Base MEV node emitter.
//!
//! Rust mirror of `packages/node-protocol/src/index.ts` in the base-mev repo —
//! the frozen single-source contract (reth-fork-contract.md §8.1, position #5).
//! [`encode_event`] produces the SAME JSON as the TS `encodeEvent` (codec.ts):
//!
//!   - `bigint` fields (`blockNumber`, `timestamp`, `balanceDeltaRaw`) are decimal
//!     STRINGS (lossless for uint256/int256);
//!   - `number` fields (`protocolVersion`, `flashblockIndex`, `flashblockCount`)
//!     stay JSON numbers;
//!   - addresses / hashes / selectors are lowercased `0x` hex (alloy serde);
//!   - `kind` is the discriminant, emitted first;
//!   - absent optional fields are omitted (matches `JSON.stringify` dropping
//!     `undefined`).
//!
//! This crate is wire-only — no EVM/ExEx logic — so byte-for-byte conformance is
//! locked down (and unit-tested against the TS encoding) before the inspector /
//! `ExEx` increments build on it.

use alloy_primitives::{Address, FixedBytes, I256, B256};
use serde::{Deserialize, Serialize};

/// Protocol version embedded in every event (mirrors `PROTOCOL_VERSION`).
pub const PROTOCOL_VERSION: u32 = 1;
/// SOFT expectation only — NOT a validation bound (index 10/11+ are valid).
pub const EXPECTED_FLASHBLOCKS_PER_BLOCK: u32 = 10;

/// 4-byte function selector; lowercased `0x` + 8 hex on the wire.
pub type Selector = FixedBytes<4>;

/// Node-integration ExEx + extension (C-1+). Behind the `node` feature so the
/// default crate stays wire-only.
#[cfg(feature = "node")]
pub mod exex;

/// `bigint` <-> decimal-string codec for unsigned 64-bit fields.
mod dec_u64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Optional `bigint` <-> decimal-string codec (omitted when `None`).
mod dec_u64_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => s.serialize_str(&n.to_string()),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        opt.map(|s| s.parse().map_err(serde::de::Error::custom)).transpose()
    }
}

/// Signed `bigint` (int256) <-> decimal-string codec for `balanceDeltaRaw`.
mod dec_i256 {
    use alloy_primitives::I256;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(v: &I256, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<I256, D::Error> {
        let s = String::deserialize(d)?;
        I256::from_dec_str(&s).map_err(serde::de::Error::custom)
    }
}

/// A single internal call captured during EVM execution (forensic detail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InternalCall {
    /// Caller address (lowercased).
    pub from: Address,
    /// Callee address (lowercased).
    pub to: Address,
    /// 4-byte function selector, lowercased.
    pub selector: Selector,
}

/// Per-tx token balance change — one per `(tx, account, token)` net storage delta
/// within a flashblock. `balance_delta_raw` is signed (negative = decrease).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateDiffEvent {
    /// Protocol version ([`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Transaction that produced the delta.
    pub tx_hash: B256,
    /// L2 block number (decimal string on the wire).
    #[serde(with = "dec_u64")]
    pub block_number: u64,
    /// Flashblock index within the block (no upper bound).
    pub flashblock_index: u32,
    /// Owning flashblock payload — REQUIRED (the finalize/discard invariant needs it).
    pub payload_id: String,
    /// Account whose token balance changed.
    pub account: Address,
    /// ERC-20 token contract.
    pub token: Address,
    /// Net signed balance delta (int256, decimal string on the wire).
    #[serde(with = "dec_i256")]
    pub balance_delta_raw: I256,
    /// Optional forensic internal calls (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub internal_calls: Option<Vec<InternalCall>>,
}

/// A flashblock preconfirmation frame. `payload_id` is the primary identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashblockEvent {
    /// Protocol version ([`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Flashblock payload identity.
    pub payload_id: String,
    /// L2 block number (decimal string on the wire).
    #[serde(with = "dec_u64")]
    pub block_number: u64,
    /// Flashblock index within the block (no upper bound).
    pub flashblock_index: u32,
    /// Parent block hash, when known (omitted otherwise).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_block_hash: Option<B256>,
    /// Block timestamp (decimal string on the wire).
    #[serde(with = "dec_u64")]
    pub timestamp: u64,
    /// State root carried by the frame.
    pub state_root: B256,
    /// Transaction hashes in the frame.
    pub tx_hashes: Vec<B256>,
    /// As-emitted finalized flag (authoritative state derives from the boundary).
    pub finalized: bool,
}

/// The 2s canonical block boundary, sealing a payload's flashblocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockBoundaryEvent {
    /// Protocol version ([`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Sealed flashblock payload identity.
    pub payload_id: String,
    /// L2 block number (decimal string on the wire).
    #[serde(with = "dec_u64")]
    pub block_number: u64,
    /// Canonical block hash at finalization.
    pub canonical_hash: B256,
    /// Emitter-reported flashblock count (the aggregator re-derives the truth).
    pub flashblock_count: u32,
    /// Whether the boundary is finalized.
    pub finalized: bool,
}

/// Reason a preconfirmed payload was discarded before finalizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscardReason {
    /// Reorged out before finalizing.
    Reorg,
    /// Superseded by a newer payload at the same height.
    Superseded,
    /// Dropped on a buffer/wall-clock timeout.
    Timeout,
}

/// Signals a preconfirmed payload was reorged/superseded before finalizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscardPreconfEvent {
    /// Protocol version ([`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Discarded flashblock payload identity.
    pub payload_id: String,
    /// L2 block number, when known (decimal string on the wire, omitted otherwise).
    #[serde(with = "dec_u64_opt", skip_serializing_if = "Option::is_none", default)]
    pub block_number: Option<u64>,
    /// Discard reason, when known (omitted otherwise).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<DiscardReason>,
}

/// Discriminated union of every event the node emits over the stream. The `kind`
/// tag is emitted first, matching the TS discriminant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeEvent {
    /// A per-tx token state diff.
    StateDiff(StateDiffEvent),
    /// A flashblock preconfirmation frame.
    Flashblock(FlashblockEvent),
    /// A canonical block boundary.
    BlockBoundary(BlockBoundaryEvent),
    /// A discarded preconfirmation.
    DiscardPreconf(DiscardPreconfEvent),
}

/// Serialize a node event to a JSON line, byte-for-byte identical to the TS
/// `encodeEvent` (bigint fields as decimal strings).
pub fn encode_event(event: &NodeEvent) -> String {
    serde_json::to_string(event).expect("NodeEvent serialization is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }
    fn hash(b: u8) -> B256 {
        B256::from([b; 32])
    }

    #[test]
    fn state_diff_encodes_like_ts() {
        let ev = NodeEvent::StateDiff(StateDiffEvent {
            protocol_version: PROTOCOL_VERSION,
            tx_hash: hash(0x22),
            block_number: 47_517_747,
            flashblock_index: 3,
            payload_id: "0x04abc".to_string(),
            account: addr(0x11),
            token: addr(0x33),
            balance_delta_raw: I256::from_dec_str("-1000").unwrap(),
            internal_calls: None,
        });
        // kind first; bigints as decimal strings; internalCalls omitted; lowercase hex.
        let expected = concat!(
            r#"{"kind":"state_diff","protocolVersion":1,"#,
            r#""txHash":"0x2222222222222222222222222222222222222222222222222222222222222222","#,
            r#""blockNumber":"47517747","flashblockIndex":3,"payloadId":"0x04abc","#,
            r#""account":"0x1111111111111111111111111111111111111111","#,
            r#""token":"0x3333333333333333333333333333333333333333","#,
            r#""balanceDeltaRaw":"-1000"}"#
        );
        assert_eq!(encode_event(&ev), expected);
    }

    #[test]
    fn state_diff_with_internal_calls() {
        let ev = NodeEvent::StateDiff(StateDiffEvent {
            protocol_version: PROTOCOL_VERSION,
            tx_hash: hash(0x22),
            block_number: 1,
            flashblock_index: 0,
            payload_id: "0x04".to_string(),
            account: addr(0x11),
            token: addr(0x33),
            balance_delta_raw: I256::from_dec_str("1000").unwrap(),
            internal_calls: Some(vec![InternalCall {
                from: addr(0xaa),
                to: addr(0xbb),
                selector: FixedBytes::<4>::from([0xde, 0xad, 0xbe, 0xef]),
            }]),
        });
        let json = encode_event(&ev);
        assert!(json.contains(r#""internalCalls":[{"from":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","to":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","selector":"0xdeadbeef"}]"#));
        assert!(json.contains(r#""balanceDeltaRaw":"1000""#));
    }

    #[test]
    fn flashblock_block_boundary_discard_encode() {
        let fb = NodeEvent::Flashblock(FlashblockEvent {
            protocol_version: PROTOCOL_VERSION,
            payload_id: "0x04".to_string(),
            block_number: 100,
            flashblock_index: 11, // high index is valid (no upper bound)
            parent_block_hash: None,
            timestamp: 1_781_781_000,
            state_root: hash(0x44),
            tx_hashes: vec![hash(0x22)],
            finalized: false,
        });
        assert_eq!(
            encode_event(&fb),
            concat!(
                r#"{"kind":"flashblock","protocolVersion":1,"payloadId":"0x04","blockNumber":"100","#,
                r#""flashblockIndex":11,"timestamp":"1781781000","#,
                r#""stateRoot":"0x4444444444444444444444444444444444444444444444444444444444444444","#,
                r#""txHashes":["0x2222222222222222222222222222222222222222222222222222222222222222"],"finalized":false}"#
            )
        );

        let bb = NodeEvent::BlockBoundary(BlockBoundaryEvent {
            protocol_version: PROTOCOL_VERSION,
            payload_id: "0x04".to_string(),
            block_number: 100,
            canonical_hash: hash(0x55),
            flashblock_count: 12,
            finalized: true,
        });
        assert!(encode_event(&bb).starts_with(r#"{"kind":"block_boundary","protocolVersion":1"#));

        let dp = NodeEvent::DiscardPreconf(DiscardPreconfEvent {
            protocol_version: PROTOCOL_VERSION,
            payload_id: "0x04".to_string(),
            block_number: Some(100),
            reason: Some(DiscardReason::Reorg),
        });
        assert_eq!(
            encode_event(&dp),
            r#"{"kind":"discard_preconf","protocolVersion":1,"payloadId":"0x04","blockNumber":"100","reason":"reorg"}"#
        );

        // Optional fields omitted when absent.
        let dp_min = NodeEvent::DiscardPreconf(DiscardPreconfEvent {
            protocol_version: PROTOCOL_VERSION,
            payload_id: "0x04".to_string(),
            block_number: None,
            reason: None,
        });
        assert_eq!(
            encode_event(&dp_min),
            r#"{"kind":"discard_preconf","protocolVersion":1,"payloadId":"0x04"}"#
        );
    }

    #[test]
    fn round_trips_through_decode() {
        let ev = NodeEvent::StateDiff(StateDiffEvent {
            protocol_version: PROTOCOL_VERSION,
            tx_hash: hash(0x22),
            block_number: 47_517_747,
            flashblock_index: 3,
            payload_id: "0x04abc".to_string(),
            account: addr(0x11),
            token: addr(0x33),
            balance_delta_raw: I256::from_dec_str("-123456789012345678901234567890").unwrap(),
            internal_calls: None,
        });
        let json = encode_event(&ev);
        let back: NodeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }
}
