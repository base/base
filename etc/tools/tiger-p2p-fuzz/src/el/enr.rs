//! ENR target: tiger's `Enr::decode` (decode and v4 signature check) against the `enr` crate's
//! `Enr<CombinedKey>`, the type sigp `discv5` (and so base-reth) decodes records with.
//!
//! Records are built and signed by the harness; mutations either break the record after signing
//! or change its content and sign again, so decoding and verification are both reached.

use alloy_primitives::{hex, keccak256};
use alloy_rlp::Decodable;
use discv5::enr::{CombinedKey, Enr as RethEnr};
use secp256k1::{Message, SECP256K1, SecretKey};

use super::{
    decode::{Seed, Verdict, default_mutate},
    rlp::{Item, Node, b, int, l, mutate as rlp_mutate},
    rng::Rng,
    wire::secret,
};

/// The secp256k1 group order, to build high-s signatures.
const ORDER: [u8; 32] =
    alloy_primitives::hex!("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");

fn content(rng: &mut Rng, key: &SecretKey) -> Node {
    let mut pairs: Vec<(Vec<u8>, Node)> = vec![
        (b"id".to_vec(), b(b"v4".to_vec())),
        (b"secp256k1".to_vec(), b(key.public_key(SECP256K1).serialize().to_vec())),
    ];
    if !rng.one_in(4) {
        pairs.push((b"ip".to_vec(), b(rng.bytes(4))));
    }
    if !rng.one_in(4) {
        pairs.push((b"udp".to_vec(), int(rng.edge_u64() & 0xffff)));
    }
    if rng.one_in(2) {
        pairs.push((b"tcp".to_vec(), int(rng.edge_u64() & 0xffff)));
    }
    if rng.one_in(2) {
        // base-reth's fork id entry: `[[hash, next]]`.
        pairs.push((b"opel".to_vec(), l(vec![l(vec![b(rng.bytes(4)), int(rng.edge_u64())])])));
    }
    if rng.one_in(6) {
        pairs.push((b"eth2".to_vec(), b(rng.bytes(16))));
    }
    if rng.one_in(6) {
        pairs.push((b"ip6".to_vec(), b(rng.bytes(16))));
    }
    if rng.one_in(8) {
        pairs.push((b"zz".to_vec(), b(rng.bytes_below(200))));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut items = vec![int(rng.edge_u64())];
    for (k, v) in pairs {
        items.push(b(k));
        items.push(v);
    }
    l(items)
}

/// Signs `content` (`[seq, k, v, ...]`) and returns the record `[sig, seq, k, v, ...]`.
fn sign(key: &SecretKey, content: &Node, high_s: bool) -> Node {
    let digest = keccak256(content.encode());
    let sig = SECP256K1.sign_ecdsa(&Message::from_digest(digest.0), key).serialize_compact();
    let mut sig = sig.to_vec();
    if high_s {
        // s' = n - s: the same signature, malleated.
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let v = ORDER[i] as i16 - sig[32 + i] as i16 - borrow;
            borrow = i16::from(v < 0);
            sig[32 + i] = v.rem_euclid(256) as u8;
        }
    }
    let Item::List(items) = &content.item else { unreachable!("content is a list") };
    let mut record = vec![b(sig)];
    record.extend(items.iter().cloned());
    l(record)
}

pub(crate) fn generate(rng: &mut Rng) -> Seed {
    let key = secret(rng);
    let content = content(rng, &key);
    let record = sign(&key, &content, false);
    let mut seed = Seed::tree("record", record);
    seed.aux = key.secret_bytes().to_vec();
    seed.aux.extend(content.encode());
    seed
}

pub(crate) fn mutate(rng: &mut Rng, seed: &Seed) -> (Vec<u8>, String) {
    let key = SecretKey::from_slice(&seed.aux[..32]).unwrap();
    let content = super::rlp::parse(&seed.aux[32..]).unwrap().0;
    match rng.below(4) {
        0 => default_mutate(rng, seed),
        1 => {
            // Change the content, then sign what was changed.
            let (bytes, name) = rlp_mutate(rng, &content);
            match super::rlp::parse(&bytes) {
                Some((node, used)) if used == bytes.len() && matches!(node.item, Item::List(_)) => {
                    (sign(&key, &node, false).encode(), format!("resigned:{name}"))
                }
                _ => default_mutate(rng, seed),
            }
        }
        2 => (sign(&key, &content, true).encode(), "high-s".to_string()),
        _ => {
            // Grow the record past 300 bytes with a padding key, signed.
            let Item::List(mut items) = content.item.clone() else { unreachable!() };
            items.push(b(b"zzz".to_vec()));
            items.push(b(rng.bytes_below(150).into_iter().chain(rng.bytes(150)).collect()));
            (sign(&key, &l(items), false).encode(), "oversize".to_string())
        }
    }
}

pub(crate) fn check(_case: &str, input: &[u8]) -> (Verdict, Verdict) {
    let tiger = match base_tiger_net::Enr::decode(&mut &input[..]) {
        Ok(enr) => {
            let pairs: Vec<String> = enr
                .pairs
                .iter()
                .map(|(k, v)| format!("{}={}", hex::encode(k), hex::encode(v)))
                .collect();
            Verdict::ok(format!("seq={} node={} {}", enr.seq, enr.node_id(), pairs.join(",")))
        }
        Err(e) => Verdict::err(e),
    };
    let reth = match RethEnr::<CombinedKey>::decode(&mut &input[..]) {
        Ok(enr) => {
            let pairs: Vec<String> =
                enr.iter().map(|(k, v)| format!("{}={}", hex::encode(k), hex::encode(v))).collect();
            Verdict::ok(format!(
                "seq={} node=0x{} {}",
                enr.seq(),
                hex::encode(enr.node_id().raw()),
                pairs.join(",")
            ))
        }
        Err(e) => Verdict::err(e),
    };
    (tiger, reth)
}
