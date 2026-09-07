use std::sync::Arc;

use alloy_primitives::{Address, FixedBytes, U128, U256};
use ssz::{Bitfield, Fixed, Variable};
use typenum::Unsigned;

use super::*;

fn int_to_hash256(int: u64) -> Hash256 {
    let mut bytes = [0; HASHSIZE];
    bytes[0..8].copy_from_slice(&int.to_le_bytes());
    Hash256::from_slice(&bytes)
}

macro_rules! impl_for_bitsize {
    ($type: ident, $bit_size: expr) => {
        impl TreeHash for $type {
            fn tree_hash_type() -> TreeHashType {
                TreeHashType::Basic
            }

            fn tree_hash_packed_encoding(&self) -> PackedEncoding {
                PackedEncoding::from_slice(&self.to_le_bytes())
            }

            fn tree_hash_packing_factor() -> usize {
                HASHSIZE / ($bit_size / 8)
            }

            #[allow(clippy::cast_lossless)] // Lint does not apply to all uses of this macro.
            fn tree_hash_root(&self) -> Hash256 {
                int_to_hash256(*self as u64)
            }
        }
    };
}

impl_for_bitsize!(u8, 8);
impl_for_bitsize!(u16, 16);
impl_for_bitsize!(u32, 32);
impl_for_bitsize!(u64, 64);
impl_for_bitsize!(usize, 64);

impl TreeHash for bool {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Basic
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        (*self as u8).tree_hash_packed_encoding()
    }

    fn tree_hash_packing_factor() -> usize {
        u8::tree_hash_packing_factor()
    }

    fn tree_hash_root(&self) -> Hash256 {
        int_to_hash256(*self as u64)
    }
}

impl<const N: usize> TreeHash for [u8; N] {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Vector
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_root(&self) -> Hash256 {
        let values_per_chunk = BYTES_PER_CHUNK;
        let minimum_chunk_count = N.div_ceil(values_per_chunk);
        merkle_root(self, minimum_chunk_count)
    }
}

impl TreeHash for U128 {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Basic
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        PackedEncoding::from_slice(&self.to_le_bytes::<{ Self::BYTES }>())
    }

    fn tree_hash_packing_factor() -> usize {
        2
    }

    fn tree_hash_root(&self) -> Hash256 {
        Hash256::right_padding_from(&self.to_le_bytes::<{ Self::BYTES }>())
    }
}

impl TreeHash for U256 {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Basic
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        PackedEncoding::from(self.to_le_bytes::<{ Self::BYTES }>())
    }

    fn tree_hash_packing_factor() -> usize {
        1
    }

    fn tree_hash_root(&self) -> Hash256 {
        Hash256::from(self.to_le_bytes::<{ Self::BYTES }>())
    }
}

impl TreeHash for Address {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Vector
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_root(&self) -> Hash256 {
        let mut result = [0; 32];
        result[0..20].copy_from_slice(self.as_slice());
        Hash256::from_slice(&result)
    }
}

// This implementation covers `Hash256`/`B256` as well.
impl<const N: usize> TreeHash for FixedBytes<N> {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Vector
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_root(&self) -> Hash256 {
        self.0.tree_hash_root()
    }
}

impl<T: TreeHash> TreeHash for Arc<T> {
    fn tree_hash_type() -> TreeHashType {
        T::tree_hash_type()
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        self.as_ref().tree_hash_packed_encoding()
    }

    fn tree_hash_packing_factor() -> usize {
        T::tree_hash_packing_factor()
    }

    fn tree_hash_root(&self) -> Hash256 {
        self.as_ref().tree_hash_root()
    }
}

/// A helper function providing common functionality for finding the Merkle root of some bytes that
/// represent a bitfield.
pub fn bitfield_bytes_tree_hash_root<N: Unsigned>(bytes: &[u8]) -> Hash256 {
    let byte_size = N::to_usize().div_ceil(8);
    let leaf_count = byte_size.div_ceil(BYTES_PER_CHUNK);

    let mut hasher = MerkleHasher::with_leaves(leaf_count);

    hasher.write(bytes).expect("bitfield should not exceed tree hash leaf limit");

    hasher.finish().expect("bitfield tree hash buffer should not exceed leaf limit")
}

impl<N: Unsigned + Clone> TreeHash for Bitfield<Variable<N>> {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::List
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        unreachable!("List should never be packed.")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("List should never be packed.")
    }

    fn tree_hash_root(&self) -> Hash256 {
        // Note: we use `as_slice` because it does _not_ have the length-delimiting bit set (or
        // present).
        let root = bitfield_bytes_tree_hash_root::<N>(self.as_slice());
        mix_in_length(&root, self.len())
    }
}

impl<N: Unsigned + Clone> TreeHash for Bitfield<Fixed<N>> {
    fn tree_hash_type() -> TreeHashType {
        TreeHashType::Vector
    }

    fn tree_hash_packed_encoding(&self) -> PackedEncoding {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_packing_factor() -> usize {
        unreachable!("Vector should never be packed.")
    }

    fn tree_hash_root(&self) -> Hash256 {
        bitfield_bytes_tree_hash_root::<N>(self.as_slice())
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use ssz::{BitList, BitVector};
    use typenum::{U8, U32};

    use super::*;

    #[test]
    fn bool() {
        let mut true_bytes: Vec<u8> = vec![1];
        true_bytes.append(&mut vec![0; 31]);

        let false_bytes: Vec<u8> = vec![0; 32];

        assert_eq!(true.tree_hash_root().as_slice(), true_bytes.as_slice());
        assert_eq!(false.tree_hash_root().as_slice(), false_bytes.as_slice());
    }

    #[test]
    fn arc() {
        let one = U128::from(1);
        let one_arc = Arc::new(one);
        assert_eq!(one_arc.tree_hash_root(), one.tree_hash_root());
    }

    #[test]
    fn int_to_bytes() {
        assert_eq!(int_to_hash256(0).as_slice(), &[0; 32]);
        assert_eq!(
            int_to_hash256(1).as_slice(),
            &[
                1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0
            ]
        );
        assert_eq!(
            int_to_hash256(u64::max_value()).as_slice(),
            &[
                255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn bitvector() {
        let empty_bitvector = BitVector::<U8>::new();
        assert_eq!(empty_bitvector.tree_hash_root(), Hash256::ZERO);

        let small_bitvector_bytes = vec![0xff_u8, 0xee, 0xdd, 0xcc];
        let small_bitvector =
            BitVector::<U32>::from_bytes(small_bitvector_bytes.clone().into()).unwrap();
        assert_eq!(small_bitvector.tree_hash_root().as_slice()[..4], small_bitvector_bytes);
    }

    #[test]
    fn bitlist() {
        let empty_bitlist = BitList::<U8>::with_capacity(8).unwrap();
        assert_eq!(
            empty_bitlist.tree_hash_root(),
            Hash256::from_str("0x5ac78d953211aa822c3ae6e9b0058e42394dd32e5992f29f9c12da3681985130")
                .unwrap()
        );

        let mut small_bitlist = BitList::<U32>::with_capacity(4).unwrap();
        small_bitlist.set(1, true).unwrap();
        assert_eq!(
            small_bitlist.tree_hash_root(),
            Hash256::from_str("0x7eb03d394d83a389980b79897207be3a6512d964cb08978bb7f3cfc0db8cfb8a")
                .unwrap()
        );
    }

    #[test]
    fn fixed_bytes_7() {
        let data = [
            [0, 1, 2, 3, 4, 5, 6],
            [6, 5, 4, 3, 2, 1, 0],
            [0, 0, 0, 0, 0, 0, 0],
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        ];
        for bytes in data {
            assert_eq!(bytes.tree_hash_root(), Hash256::right_padding_from(&bytes));
        }
    }

    #[test]
    fn address() {
        let data = [
            Address::ZERO,
            Address::repeat_byte(0xff),
            Address::right_padding_from(&[0, 1, 2, 3, 4, 5]),
            Address::left_padding_from(&[10, 9, 8, 7, 6]),
        ];
        for address in data {
            assert_eq!(address.tree_hash_root(), Hash256::right_padding_from(address.as_slice()));
        }
    }

    #[test]
    fn fixed_bytes_32() {
        let data = [
            Hash256::ZERO,
            Hash256::repeat_byte(0xff),
            Hash256::right_padding_from(&[0, 1, 2, 3, 4, 5]),
            Hash256::left_padding_from(&[10, 9, 8, 7, 6]),
        ];
        for bytes in data {
            assert_eq!(bytes.tree_hash_root(), bytes);
        }
    }

    #[test]
    fn fixed_bytes_48() {
        let data = [
            (
                FixedBytes::<48>::ZERO,
                "0xf5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b",
            ),
            (
                FixedBytes::<48>::repeat_byte(0xff),
                "0x1e3915ef9ca4ed8619d472b72fb1833448756054b4de9acb439da54dff7166aa",
            ),
        ];
        for (bytes, expected) in data {
            assert_eq!(bytes.tree_hash_root(), Hash256::from_str(expected).unwrap());
        }
    }

    // Only basic types should be packed.
    #[test]
    #[should_panic]
    fn fixed_bytes_no_packed_encoding() {
        Hash256::ZERO.tree_hash_packed_encoding();
    }

    #[test]
    #[should_panic]
    fn fixed_bytes_no_packing_factor() {
        Hash256::tree_hash_packing_factor();
    }

    #[test]
    #[should_panic]
    fn address_no_packed_encoding() {
        Address::ZERO.tree_hash_packed_encoding();
    }

    #[test]
    #[should_panic]
    fn address_no_packing_factor() {
        Address::tree_hash_packing_factor();
    }
}
