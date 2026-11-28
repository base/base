//! An RLP tree that can be re-encoded in non-canonical forms, and the structured mutations the
//! decode targets apply to valid messages.

use super::rng::Rng;

/// How one item is encoded: canonically, or in one of the forms decoders must reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quirk {
    /// The canonical encoding.
    None,
    /// The long-form length header for a payload shorter than 56 bytes (`0xb8 n` / `0xf8 n`).
    LongForm,
    /// A single byte below 0x80 wrapped as a one-byte string (`0x81 b`).
    WrapSingle,
    /// The length header declares `delta` more (or fewer) payload bytes than follow.
    LengthDelta(i8),
}

/// An RLP item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    /// A byte string.
    Bytes(Vec<u8>),
    /// A list.
    List(Vec<Node>),
}

/// An RLP item with its encoding quirk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    /// The item.
    pub(crate) item: Item,
    /// How to encode it.
    pub(crate) quirk: Quirk,
}

/// A byte string node.
pub(crate) const fn b(bytes: Vec<u8>) -> Node {
    Node { item: Item::Bytes(bytes), quirk: Quirk::None }
}

/// A list node.
pub(crate) const fn l(items: Vec<Node>) -> Node {
    Node { item: Item::List(items), quirk: Quirk::None }
}

/// The canonical integer node (big-endian, no leading zeros).
pub(crate) fn int(value: u64) -> Node {
    let bytes = value.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(8);
    b(bytes[start..].to_vec())
}

/// Parses one item (canonical or not) from the start of `buf`; returns it and the bytes used.
pub(crate) fn parse(buf: &[u8]) -> Option<(Node, usize)> {
    let first = *buf.first()?;
    let (list, header, len) = match first {
        0x00..=0x7f => return Some((b(vec![first]), 1)),
        0x80..=0xb7 => (false, 1, (first - 0x80) as usize),
        0xb8..=0xbf => {
            let n = (first - 0xb7) as usize;
            (false, 1 + n, be_len(buf.get(1..1 + n)?)?)
        }
        0xc0..=0xf7 => (true, 1, (first - 0xc0) as usize),
        0xf8..=0xff => {
            let n = (first - 0xf7) as usize;
            (true, 1 + n, be_len(buf.get(1..1 + n)?)?)
        }
    };
    let payload = buf.get(header..header.checked_add(len)?)?;
    let item = if list {
        let mut items = Vec::new();
        let mut rest = payload;
        while !rest.is_empty() {
            let (node, used) = parse(rest)?;
            items.push(node);
            rest = &rest[used..];
        }
        Item::List(items)
    } else {
        Item::Bytes(payload.to_vec())
    };
    Some((Node { item, quirk: Quirk::None }, header + len))
}

fn be_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() > 4 {
        return None;
    }
    Some(bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize))
}

fn put_header(out: &mut Vec<u8>, list: bool, len: usize, quirk: Quirk) {
    let (short, long) = if list { (0xc0u8, 0xf7u8) } else { (0x80u8, 0xb7u8) };
    let declared = match quirk {
        Quirk::LengthDelta(d) => (len as i64 + d as i64).max(0) as usize,
        _ => len,
    };
    if declared < 56 && quirk != Quirk::LongForm {
        out.push(short + declared as u8);
    } else {
        let bytes = (declared as u64).to_be_bytes();
        let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
        out.push(long + (8 - start) as u8);
        out.extend_from_slice(&bytes[start..]);
    }
}

impl Node {
    /// Appends this node's encoding to `out`.
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        match &self.item {
            Item::Bytes(bytes) => {
                if bytes.len() == 1 && bytes[0] < 0x80 && self.quirk == Quirk::None {
                    out.push(bytes[0]);
                } else {
                    put_header(out, false, bytes.len(), self.quirk);
                    out.extend_from_slice(bytes);
                }
            }
            Item::List(items) => {
                let mut payload = Vec::new();
                for item in items {
                    item.encode_into(&mut payload);
                }
                put_header(out, true, payload.len(), self.quirk);
                out.extend_from_slice(&payload);
            }
        }
    }

    /// This node's encoding.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn paths(&self, prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        out.push(prefix.clone());
        if let Item::List(items) = &self.item {
            for (i, item) in items.iter().enumerate() {
                prefix.push(i);
                item.paths(prefix, out);
                prefix.pop();
            }
        }
    }

    fn get(&self, path: &[usize]) -> &Self {
        match (path.split_first(), &self.item) {
            (None, _) => self,
            (Some((i, rest)), Item::List(items)) => items[*i].get(rest),
            (Some(_), Item::Bytes(_)) => unreachable!("paths only lead through lists"),
        }
    }

    fn at(&mut self, path: &[usize]) -> &mut Self {
        match path.split_first() {
            None => self,
            Some((i, rest)) => match &mut self.item {
                Item::List(items) => items[*i].at(rest),
                Item::Bytes(_) => unreachable!("paths only lead through lists"),
            },
        }
    }
}

/// The structured mutations, by name (the name is part of a disagreement's signature).
pub(crate) const MUTATIONS: [&str; 20] = [
    "truncate",
    "trailing-bytes",
    "leading-zero",
    "wrap-single-byte",
    "long-form-length",
    "length-plus-one",
    "length-minus-one",
    "list-to-string",
    "string-to-list",
    "extra-list-element",
    "drop-list-element",
    "duplicate-list-element",
    "empty-item",
    "oversize-integer",
    "max-integer",
    "random-string",
    "byte-flip",
    "byte-insert",
    "byte-delete",
    "replace-with-sibling",
];

/// Applies one random structured mutation to the valid encoding `tree`. Returns the mutated
/// bytes and the mutation's name.
pub(crate) fn mutate(rng: &mut Rng, tree: &Node) -> (Vec<u8>, &'static str) {
    let name = *rng.pick(&MUTATIONS);
    let mut tree = tree.clone();
    let mut paths = Vec::new();
    tree.paths(&mut Vec::new(), &mut paths);
    let path = rng.pick(&paths).clone();
    let bytes_paths: Vec<Vec<usize>> =
        paths.iter().filter(|p| matches!(tree.get(p).item, Item::Bytes(_))).cloned().collect();
    let list_paths: Vec<Vec<usize>> =
        paths.iter().filter(|p| matches!(tree.get(p).item, Item::List(_))).cloned().collect();
    let pick_bytes = |rng: &mut Rng| bytes_paths.get(rng.below(bytes_paths.len().max(1))).cloned();
    match name {
        "truncate" => {
            let enc = tree.encode();
            let cut = rng.below(enc.len().max(1));
            return (enc[..cut].to_vec(), name);
        }
        "trailing-bytes" => {
            let mut enc = tree.encode();
            let n = 1 + rng.below(4);
            enc.extend(rng.bytes(n));
            return (enc, name);
        }
        "byte-flip" | "byte-insert" | "byte-delete" => {
            let mut enc = tree.encode();
            if enc.is_empty() {
                return (vec![rng.next_u64() as u8], name);
            }
            let at = rng.below(enc.len());
            match name {
                "byte-flip" => enc[at] ^= 1 << rng.below(8),
                "byte-insert" => enc.insert(at, rng.next_u64() as u8),
                _ => {
                    enc.remove(at);
                }
            }
            return (enc, name);
        }
        "leading-zero" => {
            if let Some(p) = pick_bytes(rng)
                && let Item::Bytes(bytes) = &mut tree.at(&p).item
            {
                bytes.insert(0, 0);
            }
        }
        "wrap-single-byte" => {
            if let Some(p) = pick_bytes(rng) {
                let node = tree.at(&p);
                if let Item::Bytes(bytes) = &mut node.item
                    && !(bytes.len() == 1 && bytes[0] < 0x80)
                {
                    *bytes = vec![rng.below(0x80) as u8];
                }
                node.quirk = Quirk::WrapSingle;
            }
        }
        "long-form-length" => tree.at(&path).quirk = Quirk::LongForm,
        "length-plus-one" => tree.at(&path).quirk = Quirk::LengthDelta(1),
        "length-minus-one" => tree.at(&path).quirk = Quirk::LengthDelta(-1),
        "list-to-string" => {
            if let Some(p) = list_paths.get(rng.below(list_paths.len().max(1))) {
                let node = tree.at(p);
                let payload = match &node.item {
                    Item::List(items) => items.iter().flat_map(Node::encode).collect(),
                    Item::Bytes(bytes) => bytes.clone(),
                };
                node.item = Item::Bytes(payload);
            }
        }
        "string-to-list" => {
            if let Some(p) = pick_bytes(rng) {
                let node = tree.at(&p);
                if let Item::Bytes(bytes) = &node.item {
                    let items = parse(bytes)
                        .filter(|(_, used)| *used == bytes.len())
                        .map_or_else(|| vec![b(bytes.clone())], |(n, _)| vec![n]);
                    node.item = Item::List(items);
                }
            }
        }
        "extra-list-element" | "drop-list-element" | "duplicate-list-element" => {
            if let Some(p) = list_paths.get(rng.below(list_paths.len().max(1))) {
                let extra = if rng.one_in(2) { int(rng.edge_u64()) } else { b(rng.bytes(32)) };
                if let Item::List(items) = &mut tree.at(p).item {
                    match name {
                        "extra-list-element" => items.push(extra),
                        "drop-list-element" => {
                            if !items.is_empty() {
                                let i = rng.below(items.len());
                                items.remove(i);
                            }
                        }
                        _ => {
                            if !items.is_empty() {
                                let i = rng.below(items.len());
                                let copy = items[i].clone();
                                items.insert(i, copy);
                            }
                        }
                    }
                }
            }
        }
        "empty-item" => {
            let node = tree.at(&path);
            node.item =
                if rng.one_in(2) { Item::Bytes(Vec::new()) } else { Item::List(Vec::new()) };
        }
        "oversize-integer" | "max-integer" | "random-string" => {
            if let Some(p) = pick_bytes(rng) {
                let len = match name {
                    "oversize-integer" => *rng.pick(&[9usize, 17, 33, 65]),
                    "max-integer" => *rng.pick(&[4usize, 8, 32]),
                    _ => rng.below(70),
                };
                let value = if name == "max-integer" { vec![0xff; len] } else { rng.bytes(len) };
                tree.at(&p).item = Item::Bytes(value);
            }
        }
        _ => {
            // replace-with-sibling: copy one node over another of the tree.
            let source = tree.get(rng.pick(&paths)).clone();
            *tree.at(&path) = source;
        }
    }
    (tree.encode(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_round_trip_and_quirks() {
        let tree = l(vec![int(0), int(0x7f), int(0x80), b(vec![1; 60]), l(vec![int(1024)])]);
        let enc = tree.encode();
        assert!(alloy_rlp::Header::decode(&mut enc.as_slice()).unwrap().list);
        assert_eq!(parse(&enc).unwrap().0, tree);
        let mut wrapped = int(5);
        wrapped.quirk = Quirk::WrapSingle;
        assert_eq!(wrapped.encode(), vec![0x81, 0x05]);
        let mut long = int(0x80);
        long.quirk = Quirk::LongForm;
        assert_eq!(long.encode(), vec![0xb8, 0x01, 0x80]);
        let mut plus = l(vec![int(1)]);
        plus.quirk = Quirk::LengthDelta(1);
        assert_eq!(plus.encode(), vec![0xc2, 0x01]);
    }
}
