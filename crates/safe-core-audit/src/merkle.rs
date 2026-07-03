pub struct MerkleTree;
pub struct MerkleProof;

impl Default for MerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

impl MerkleTree {
    pub fn new() -> Self {
        Self
    }
    pub fn push(&mut self, _hash: [u8; 32]) {}
    pub fn root(&self) -> Option<[u8; 32]> {
        None
    }
    pub fn prove(&self, _index: usize) -> Option<MerkleProof> {
        None
    }
}
