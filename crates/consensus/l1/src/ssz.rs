use alloy_primitives::B256;
use sha2::{Digest, Sha256};

/// SSZ merkleization helpers for the light-client containers.
#[derive(Debug, Clone, Copy)]
pub struct Ssz;

impl Ssz {
    /// Returns `sha256(left ++ right)`.
    pub fn hash_pair(left: &B256, right: &B256) -> B256 {
        let mut hasher = Sha256::new();
        hasher.update(left);
        hasher.update(right);
        B256::from_slice(&hasher.finalize())
    }

    /// Merkleizes `chunks` into a tree padded with zero chunks to the next power of two of
    /// `limit`.
    pub fn merkleize(chunks: &[B256], limit: usize) -> B256 {
        let depth = limit.max(1).next_power_of_two().trailing_zeros();
        let mut layer = chunks.to_vec();
        let mut zero = B256::ZERO;
        for _ in 0..depth {
            if layer.len() % 2 == 1 {
                layer.push(zero);
            }
            layer = layer.chunks_exact(2).map(|pair| Self::hash_pair(&pair[0], &pair[1])).collect();
            zero = Self::hash_pair(&zero, &zero);
        }
        layer.first().copied().unwrap_or(zero)
    }

    /// Returns the SSZ chunk of a `uint64`.
    pub fn uint64(value: u64) -> B256 {
        let mut chunk = B256::ZERO;
        chunk[..8].copy_from_slice(&value.to_le_bytes());
        chunk
    }

    /// Splits `bytes` into right-padded 32-byte chunks.
    pub fn pack(bytes: &[u8]) -> Vec<B256> {
        bytes
            .chunks(32)
            .map(|piece| {
                let mut chunk = B256::ZERO;
                chunk[..piece.len()].copy_from_slice(piece);
                chunk
            })
            .collect()
    }

    /// Splits an SSZ `Vector[Bytes32, N]` into its entries.
    pub fn chunks(bytes: &[u8]) -> Vec<B256> {
        bytes.chunks_exact(32).map(B256::from_slice).collect()
    }

    /// Returns the SSZ length in bytes of a Merkle branch to generalized index `gindex`.
    pub const fn branch_len(gindex: u64) -> usize {
        gindex.ilog2() as usize * 32
    }

    /// Returns the root of a `ByteList[max_len]`.
    pub fn byte_list(bytes: &[u8], max_len: usize) -> B256 {
        let root = Self::merkleize(&Self::pack(bytes), max_len.div_ceil(32));
        Self::hash_pair(&root, &Self::uint64(bytes.len() as u64))
    }

    /// Checks that `leaf` sits at generalized index `gindex` under `root`.
    ///
    /// The branch may carry leading zero entries, which servers add to pad it to the depth of a
    /// later fork.
    pub fn is_valid_branch(leaf: B256, branch: &[B256], gindex: u64, root: B256) -> bool {
        let depth = gindex.ilog2() as usize;
        let Some(extra) = branch.len().checked_sub(depth) else { return false };
        if !branch[..extra].iter().all(B256::is_zero) {
            return false;
        }
        let index = gindex - (1 << depth);
        let computed = branch[extra..].iter().enumerate().fold(leaf, |node, (level, sibling)| {
            if (index >> level) & 1 == 1 {
                Self::hash_pair(sibling, &node)
            } else {
                Self::hash_pair(&node, sibling)
            }
        });
        computed == root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_round_trips_through_merkleize() {
        let leaves: Vec<B256> = (0..5u64).map(Ssz::uint64).collect();
        let root = Ssz::merkleize(&leaves, 8);

        let l01 = Ssz::hash_pair(&leaves[0], &leaves[1]);
        let zero = B256::ZERO;
        let z1 = Ssz::hash_pair(&zero, &zero);
        let l45 = Ssz::hash_pair(&leaves[4], &zero);
        let right = Ssz::hash_pair(&l45, &z1);
        let branch = [leaves[3], l01, right];

        assert!(Ssz::is_valid_branch(leaves[2], &branch, 8 + 2, root));
        assert!(!Ssz::is_valid_branch(leaves[2], &branch, 8 + 3, root));
    }
}
