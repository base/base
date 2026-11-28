//! eth/68 and eth/69 message target, after the Status exchange.
//!
//! The input is the relative message id followed by the RLP payload, exactly what each side
//! decodes after decompression. tiger's side follows `handle_eth` in peer.rs with the decoders it
//! calls there (and the downloader's `decode_headers` / `decode_bodies` for the responses it
//! requests); messages tiger accepts without reading come back as `accept(ignored)`. base-reth's
//! side is `EthStreamInner::decode_message` (size cap, `ProtocolMessage::decode_message`, Status
//! after the handshake) plus the checks `ActiveSession::on_incoming_message` makes on
//! `BlockRangeUpdate`.

use alloy_consensus::Header;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::Decodable;
use base_common_consensus::{BasePooledTransaction, BasePrimitives};
use base_tiger_net::{BlockBody, BlockId, HeadersRequest, SealedHeader};
use bytes::BytesMut;
use reth_eth_wire::{EthMessage, EthStreamInner, EthVersion, primitives::BasicNetworkPrimitives};

use super::{
    decode::{Seed, Verdict, default_mutate},
    rlp::{Node, b, int, l, parse},
    rng::Rng,
};

/// base-reth's network primitives (`BaseNetworkPrimitives` in crates/execution/node).
pub(crate) type BaseNetworkPrimitives =
    BasicNetworkPrimitives<BasePrimitives, BasePooledTransaction>;

/// (relative id, name) of the messages generated.
const MESSAGES: [(u8, &str); 14] = [
    (0x01, "NewBlockHashes"),
    (0x02, "Transactions"),
    (0x03, "GetBlockHeaders"),
    (0x04, "BlockHeaders"),
    (0x05, "GetBlockBodies"),
    (0x06, "BlockBodies"),
    (0x07, "NewBlock"),
    (0x08, "NewPooledTransactionHashes"),
    (0x09, "GetPooledTransactions"),
    (0x0a, "PooledTransactions"),
    (0x0f, "GetReceipts"),
    (0x10, "Receipts"),
    (0x11, "BlockRangeUpdate"),
    (0x0b, "Unassigned"),
];

/// Ids a mutation may swap in: Status after the handshake, unassigned and removed ids, ids just
/// past each version's range, and ids far outside it.
const OTHER_IDS: [u8; 12] =
    [0x00, 0x0b, 0x0c, 0x0d, 0x0e, 0x11, 0x12, 0x13, 0x15, 0x16, 0x20, 0xff];

fn hash(rng: &mut Rng) -> Node {
    b(rng.bytes(32))
}

fn u256(rng: &mut Rng) -> Node {
    if rng.one_in(4) { b(vec![0xff; 32]) } else { int(rng.edge_u64()) }
}

fn address(rng: &mut Rng) -> Node {
    b(rng.bytes(20))
}

fn signature_tail(rng: &mut Rng, mut fields: Vec<Node>, v: u64) -> Vec<Node> {
    fields.push(int(v));
    fields.push(b(rng.bytes(32)));
    fields.push(b(rng.bytes(32)));
    fields
}

/// A transaction as it sits in a list: legacy as an RLP list, typed as a byte string.
pub(crate) fn transaction(rng: &mut Rng) -> Node {
    let data = b(rng.bytes_below(40));
    match rng.below(5) {
        0 => {
            let v = *rng.pick(&[27u64, 28, 845_300 * 2 + 35, 845_300 * 2 + 36]);
            let fields =
                vec![int(rng.edge_u64()), u256(rng), int(21_000), address(rng), u256(rng), data];
            l(signature_tail(rng, fields, v))
        }
        1 => {
            let fields = vec![
                int(845_300),
                int(rng.edge_u64()),
                u256(rng),
                u256(rng),
                int(21_000),
                address(rng),
                u256(rng),
                data,
                l(vec![]),
            ];
            let mut out = vec![0x02];
            let parity = rng.below(2) as u64;
            out.extend(l(signature_tail(rng, fields, parity)).encode());
            b(out)
        }
        2 => {
            let fields = vec![
                int(845_300),
                int(rng.edge_u64()),
                u256(rng),
                int(21_000),
                address(rng),
                u256(rng),
                data,
                l(vec![l(vec![address(rng), l(vec![hash(rng)])])]),
            ];
            let mut out = vec![0x01];
            let parity = rng.below(2) as u64;
            out.extend(l(signature_tail(rng, fields, parity)).encode());
            b(out)
        }
        _ => {
            // Base deposit: [sourceHash, from, to, mint, value, gas, isSystemTx, data].
            let mut out = vec![0x7e];
            out.extend(
                l(vec![
                    hash(rng),
                    address(rng),
                    address(rng),
                    u256(rng),
                    u256(rng),
                    int(rng.edge_u64()),
                    int(rng.below(2) as u64),
                    data,
                ])
                .encode(),
            );
            b(out)
        }
    }
}

/// A header as a tree, with a random set of the post-London fields.
pub(crate) fn header(rng: &mut Rng) -> Node {
    let optional = rng.below(7);
    let h = Header {
        parent_hash: B256::from(rng.array::<32>()),
        number: rng.edge_u64(),
        gas_limit: rng.edge_u64(),
        gas_used: rng.edge_u64(),
        timestamp: rng.edge_u64(),
        extra_data: rng.bytes_below(40).into(),
        base_fee_per_gas: (optional >= 1).then(|| rng.edge_u64()),
        withdrawals_root: (optional >= 2).then(|| B256::from(rng.array::<32>())),
        blob_gas_used: (optional >= 3).then(|| rng.edge_u64()),
        excess_blob_gas: (optional >= 3).then(|| rng.edge_u64()),
        parent_beacon_block_root: (optional >= 4).then(|| B256::from(rng.array::<32>())),
        requests_hash: (optional >= 5).then(|| B256::from(rng.array::<32>())),
        ..Default::default()
    };
    parse(&alloy_rlp::encode(&h)).unwrap().0
}

fn body(rng: &mut Rng) -> Node {
    let txs = (0..rng.below(4)).map(|_| transaction(rng)).collect();
    let mut fields = vec![l(txs), l(vec![])];
    if !rng.one_in(4) {
        fields.push(l(vec![]));
    }
    l(fields)
}

fn receipt(rng: &mut Rng, version: u64) -> Node {
    let tx_type = *rng.pick(&[0u64, 1, 2, 0x7e]);
    let log = l(vec![address(rng), l(vec![hash(rng)]), b(rng.bytes_below(40))]);
    let logs = l(if rng.one_in(2) { vec![log] } else { vec![] });
    let deposit_tail = |rng: &mut Rng, fields: &mut Vec<Node>| {
        if tx_type == 0x7e {
            fields.push(int(rng.edge_u64()));
            fields.push(int(1));
        }
    };
    if version >= 69 {
        let mut fields = vec![int(tx_type), int(rng.below(2) as u64), int(rng.edge_u64()), logs];
        deposit_tail(rng, &mut fields);
        l(fields)
    } else {
        let mut fields = vec![int(rng.below(2) as u64), int(rng.edge_u64()), b(vec![0; 256]), logs];
        deposit_tail(rng, &mut fields);
        let list = l(fields);
        if tx_type == 0 {
            list
        } else {
            let mut out = vec![tx_type as u8];
            out.extend(list.encode());
            b(out)
        }
    }
}

fn hashes(rng: &mut Rng) -> Node {
    let n = match rng.below(8) {
        0 => 0,
        1 => 1025,
        _ => rng.below(8),
    };
    let mut items: Vec<Node> = (0..n).map(|_| hash(rng)).collect();
    if n > 1 && rng.one_in(3) {
        items[1] = items[0].clone();
    }
    l(items)
}

fn request(rng: &mut Rng, body: Node) -> Node {
    l(vec![int(rng.edge_u64()), body])
}

/// The payload tree of message `id` for `version`.
fn payload(rng: &mut Rng, version: u64, id: u8) -> Node {
    match id {
        0x01 => l((0..rng.below(4)).map(|_| l(vec![hash(rng), int(rng.edge_u64())])).collect()),
        0x02 => l((0..rng.below(4)).map(|_| transaction(rng)).collect()),
        0x03 => {
            let start = if rng.one_in(2) { hash(rng) } else { int(rng.edge_u64()) };
            let reverse = int(rng.below(2) as u64);
            let fields = l(vec![start, int(rng.edge_u64()), int(rng.edge_u64()), reverse]);
            request(rng, fields)
        }
        0x04 => {
            let headers = (0..rng.below(3)).map(|_| header(rng)).collect();
            request(rng, l(headers))
        }
        0x05 | 0x09 | 0x0f => {
            let list = hashes(rng);
            request(rng, list)
        }
        0x06 => {
            let bodies = (0..rng.below(3)).map(|_| body(rng)).collect();
            request(rng, l(bodies))
        }
        0x07 => {
            let block = l(vec![header(rng), l(vec![]), l(vec![])]);
            l(vec![block, u256(rng)])
        }
        0x08 => {
            let n = rng.below(4);
            let types: Vec<u8> = (0..n).map(|_| *rng.pick(&[0u8, 1, 2, 3, 4])).collect();
            let mut sizes: Vec<Node> = (0..n).map(|_| int(rng.edge_u64() & 0xffff_ffff)).collect();
            if rng.one_in(4) {
                sizes.push(int(1));
            }
            let list = (0..n).map(|_| hash(rng)).collect();
            l(vec![b(types), l(sizes), l(list)])
        }
        0x0a => {
            let txs = (0..rng.below(3)).map(|_| transaction(rng)).collect();
            request(rng, l(txs))
        }
        0x10 => {
            let blocks = (0..rng.below(3))
                .map(|_| l((0..rng.below(3)).map(|_| receipt(rng, version)).collect()))
                .collect();
            request(rng, l(blocks))
        }
        0x11 => {
            let latest = rng.edge_u64();
            let earliest =
                if rng.one_in(4) { rng.edge_u64() } else { latest.saturating_sub(rng.edge_u64()) };
            let hash = if rng.one_in(6) { b(vec![0; 32]) } else { hash(rng) };
            l(vec![int(earliest), int(latest), hash])
        }
        _ => request(rng, l(vec![])),
    }
}

pub(crate) fn generate(rng: &mut Rng) -> Seed {
    let version = if rng.one_in(2) { 68 } else { 69 };
    let (id, name) = *rng.pick(&MESSAGES);
    let tree = payload(rng, version, id);
    let mut bytes = vec![id];
    bytes.extend(tree.encode());
    // The id byte travels outside the RLP payload, so the tree covers the payload only.
    Seed { case: format!("eth{version}/{name}"), tree: Some(tree), bytes, aux: Vec::new() }
}

pub(crate) fn mutate(rng: &mut Rng, seed: &Seed) -> (Vec<u8>, String) {
    let id = seed.bytes[0];
    if rng.one_in(12) {
        let mut out = seed.bytes.clone();
        out[0] = *rng.pick(&OTHER_IDS);
        return (out, "message-id".to_string());
    }
    let payload_seed = Seed { bytes: seed.bytes[1..].to_vec(), ..seed.clone() };
    let (payload, name) = default_mutate(rng, &payload_seed);
    let mut out = vec![id];
    out.extend(payload);
    (out, name)
}

fn hashes_digest(hashes: &[B256]) -> String {
    let joined: Vec<u8> = hashes.iter().flat_map(|h| h.0).collect();
    format!("n={} keccak={}", hashes.len(), keccak256(joined))
}

fn headers_request(id: u64, start: BlockId, limit: u64, skip: u64, reverse: bool) -> String {
    let start = match start {
        BlockId::Hash(h) => format!("hash:{h}"),
        BlockId::Number(n) => format!("number:{n}"),
    };
    format!("id={id} start={start} limit={limit} skip={skip} reverse={reverse}")
}

/// tiger's `handle_eth`, message by message.
pub(crate) fn tiger(version: u64, id: u8, payload: &[u8]) -> Verdict {
    let count = if version >= 69 { 18 } else { 17 };
    if id >= count {
        return Verdict::err("message id outside negotiated range");
    }
    let split = || base_tiger_net::split_request_id(payload);
    match id {
        0x00 => Verdict::err("second status"),
        0x03 => match split()
            .and_then(|(rid, mut body)| Ok((rid, HeadersRequest::decode(&mut body)?)))
        {
            Ok((rid, r)) => Verdict::ok(headers_request(rid, r.start, r.limit, r.skip, r.reverse)),
            Err(e) => Verdict::err(e),
        },
        0x05 | 0x0f => {
            match split().and_then(|(rid, mut body)| Ok((rid, Vec::<B256>::decode(&mut body)?))) {
                Ok((rid, list)) => Verdict::ok(format!("id={rid} hashes {}", hashes_digest(&list))),
                Err(e) => Verdict::err(e),
            }
        }
        // Responses: `Peer::deliver` reads the request id; the downloader decodes headers and
        // bodies (`decode_headers` and `decode_bodies`, which is `Vec::<BlockBody>::decode`, in
        // download.rs); receipts and pooled
        // transactions are never requested, so only the id is read.
        0x04 => {
            let decoded = split().and_then(|(rid, mut body)| {
                let mut items = alloy_rlp::Header::decode_bytes(&mut body, true)?;
                let mut hashes = Vec::new();
                while !items.is_empty() {
                    hashes.push(SealedHeader::decode(&mut items)?.hash);
                }
                Ok((rid, hashes))
            });
            match decoded {
                Ok((rid, hashes)) => {
                    Verdict::ok(format!("id={rid} headers {}", hashes_digest(&hashes)))
                }
                Err(e) => Verdict::err(e),
            }
        }
        0x06 => match split()
            .and_then(|(rid, mut body)| Ok((rid, Vec::<BlockBody>::decode(&mut body)?)))
        {
            Ok((rid, bodies)) => {
                let hashes: Vec<B256> =
                    bodies.iter().map(|b| keccak256(alloy_rlp::encode(b))).collect();
                Verdict::ok(format!("id={rid} bodies {}", hashes_digest(&hashes)))
            }
            Err(e) => Verdict::err(e),
        },
        0x09 | 0x0a | 0x10 => match split() {
            Ok((rid, _)) => Verdict::ok(format!("id={rid}")),
            Err(e) => Verdict::err(e),
        },
        0x11 => {
            let mut buf = payload;
            let decoded = alloy_rlp::Header::decode_bytes(&mut buf, true).and_then(|mut items| {
                Ok((u64::decode(&mut items)?, u64::decode(&mut items)?, B256::decode(&mut items)?))
            });
            match decoded {
                Ok((earliest, latest, hash)) => {
                    Verdict::ok(format!("range {earliest}..{latest} {hash}"))
                }
                Err(e) => Verdict::err(e),
            }
        }
        _ => Verdict::ignored(),
    }
}

/// base-reth's stream decode plus the session's checks.
pub(crate) fn reth(version: u64, input: &[u8]) -> Verdict {
    let count = if version >= 69 { 18 } else { 17 };
    if input.first().is_some_and(|id| *id >= count) {
        // P2PStream: no shared capability claims the id.
        return Verdict::err("UnknownSubprotocolMessageId");
    }
    let version = if version >= 69 { EthVersion::Eth69 } else { EthVersion::Eth68 };
    let message = match EthStreamInner::<BaseNetworkPrimitives>::new(version)
        .decode_message(BytesMut::from(input))
    {
        Ok(m) => m,
        Err(e) => return Verdict::err(e),
    };
    match message {
        EthMessage::GetBlockHeaders(r) => Verdict::ok(headers_request(
            r.request_id,
            match r.message.start_block {
                alloy_eips::BlockHashOrNumber::Hash(h) => BlockId::Hash(h),
                alloy_eips::BlockHashOrNumber::Number(n) => BlockId::Number(n),
            },
            r.message.limit,
            r.message.skip as u64,
            r.message.direction.is_falling(),
        )),
        EthMessage::GetBlockBodies(r) => {
            Verdict::ok(format!("id={} hashes {}", r.request_id, hashes_digest(&r.message.0)))
        }
        EthMessage::GetReceipts(r) => {
            Verdict::ok(format!("id={} hashes {}", r.request_id, hashes_digest(&r.message.0)))
        }
        EthMessage::BlockHeaders(r) => {
            let hashes: Vec<B256> = r.message.0.iter().map(Header::hash_slow).collect();
            Verdict::ok(format!("id={} headers {}", r.request_id, hashes_digest(&hashes)))
        }
        EthMessage::BlockBodies(r) => {
            let hashes: Vec<B256> =
                r.message.0.iter().map(|b| keccak256(alloy_rlp::encode(b))).collect();
            Verdict::ok(format!("id={} bodies {}", r.request_id, hashes_digest(&hashes)))
        }
        EthMessage::GetPooledTransactions(r) => Verdict::ok(format!("id={}", r.request_id)),
        EthMessage::PooledTransactions(r) => Verdict::ok(format!("id={}", r.request_id)),
        EthMessage::Receipts(r) => Verdict::ok(format!("id={}", r.request_id)),
        EthMessage::Receipts69(r) => Verdict::ok(format!("id={}", r.request_id)),
        EthMessage::BlockRangeUpdate(m) => {
            if m.earliest > m.latest {
                return Verdict::err("BadMessage: invalid block range: earliest > latest");
            }
            if m.latest_hash.is_zero() {
                return Verdict::err("BadMessage: latest_hash cannot be zero");
            }
            Verdict::ok(format!("range {}..{} {}", m.earliest, m.latest, m.latest_hash))
        }
        other => Verdict::ok(format!("{:?}", other.message_id())),
    }
}

pub(crate) fn check(case: &str, input: &[u8]) -> (Verdict, Verdict) {
    let version = if case.starts_with("eth69") { 69 } else { 68 };
    let Some((&id, payload)) = input.split_first() else {
        return (Verdict::err("empty"), Verdict::err("empty"));
    };
    (tiger(version, id, payload), reth(version, input))
}
