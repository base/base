use alloy_primitives::{Address, U128, U160, U256};
use ssz_derive::Encode;
use tree_hash::{BYTES_PER_CHUNK, Hash256, MerkleHasher, PackedEncoding, TreeHash};
use tree_hash_derive::TreeHash;

#[derive(Encode)]
struct HashVec {
    vec: Vec<u8>,
}

impl From<Vec<u8>> for HashVec {
    fn from(vec: Vec<u8>) -> Self {
        Self { vec }
    }
}

impl tree_hash::TreeHash for HashVec {
    fn tree_hash_type() -> tree_hash::TreeHashType {
        tree_hash::TreeHashType::List
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        unreachable!("List should never be packed.")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("List should never be packed.")
    }

    fn tree_hash_root(&self) -> Hash256 {
        let mut hasher = MerkleHasher::with_leaves(self.vec.len().div_ceil(BYTES_PER_CHUNK));

        for item in &self.vec {
            hasher.write(&item.tree_hash_packed_encoding()).unwrap()
        }

        let root = hasher.finish().unwrap();

        tree_hash::mix_in_length(&root, self.vec.len())
    }
}

fn mix_in_selector(a: Hash256, selector: u8) -> Hash256 {
    let mut b = [0; 32];
    b[0] = selector;

    Hash256::from_slice(&ethereum_hashing::hash32_concat(a.as_slice(), &b))
}

fn u8_hash_concat(v1: u8, v2: u8) -> Hash256 {
    let mut a = [0; 32];
    let mut b = [0; 32];

    a[0] = v1;
    b[0] = v2;

    Hash256::from_slice(&ethereum_hashing::hash32_concat(&a, &b))
}

fn u8_hash(x: u8) -> Hash256 {
    let mut a = [0; 32];
    a[0] = x;
    Hash256::from_slice(&a)
}

#[derive(TreeHash)]
#[tree_hash(enum_behaviour = "transparent")]
enum FixedTrans {
    A(u8),
    B(u8),
}

#[test]
fn fixed_trans() {
    assert_eq!(FixedTrans::A(2).tree_hash_root(), u8_hash(2));
    assert_eq!(FixedTrans::B(2).tree_hash_root(), u8_hash(2));
}

#[derive(TreeHash)]
#[tree_hash(enum_behaviour = "union")]
enum FixedUnion {
    A(u8),
    B(u8),
}

#[test]
fn fixed_union() {
    assert_eq!(FixedUnion::A(2).tree_hash_root(), u8_hash_concat(2, 0));
    assert_eq!(FixedUnion::B(2).tree_hash_root(), u8_hash_concat(2, 1));
}

#[derive(TreeHash)]
#[tree_hash(enum_behaviour = "transparent")]
enum VariableTrans {
    A(HashVec),
    B(HashVec),
}

#[test]
fn variable_trans() {
    assert_eq!(VariableTrans::A(HashVec::from(vec![2])).tree_hash_root(), u8_hash_concat(2, 1));
    assert_eq!(VariableTrans::B(HashVec::from(vec![2])).tree_hash_root(), u8_hash_concat(2, 1));
}

#[derive(TreeHash)]
#[tree_hash(enum_behaviour = "union")]
enum VariableUnion {
    A(HashVec),
    B(HashVec),
}

#[test]
fn variable_union() {
    assert_eq!(
        VariableUnion::A(HashVec::from(vec![2])).tree_hash_root(),
        mix_in_selector(u8_hash_concat(2, 1), 0)
    );
    assert_eq!(
        VariableUnion::B(HashVec::from(vec![2])).tree_hash_root(),
        mix_in_selector(u8_hash_concat(2, 1), 1)
    );
}

/// Test that the packed encodings for different types are equal.
#[test]
fn packed_encoding_example() {
    let val = 0xfff0eee0ddd0ccc0bbb0aaa099908880_u128;
    let canonical = U256::from(val).tree_hash_packed_encoding();
    let encodings = [
        (0x8880_u16.tree_hash_packed_encoding(), 0),
        (0x9990_u16.tree_hash_packed_encoding(), 2),
        (0xaaa0_u16.tree_hash_packed_encoding(), 4),
        (0xbbb0_u16.tree_hash_packed_encoding(), 6),
        (0xccc0_u16.tree_hash_packed_encoding(), 8),
        (0xddd0_u16.tree_hash_packed_encoding(), 10),
        (0xeee0_u16.tree_hash_packed_encoding(), 12),
        (0xfff0_u16.tree_hash_packed_encoding(), 14),
        (U128::from(val).tree_hash_packed_encoding(), 0),
        (U128::from(0).tree_hash_packed_encoding(), 16),
        (Hash256::from_slice(U256::from(val).as_le_slice()).tree_hash_root().0.into(), 0),
        (U256::from(val).tree_hash_root().0.into(), 0),
        (Address::from(U160::from(val).to_le_bytes::<20>()).tree_hash_root().0.into(), 0),
    ];
    for (i, (encoding, offset)) in encodings.into_iter().enumerate() {
        assert_eq!(
            &encoding[..],
            &canonical[offset..offset + encoding.len()],
            "encoding {i} is wrong"
        );
    }
}
