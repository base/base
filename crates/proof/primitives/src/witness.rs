use alloc::vec::Vec;

use base_proof_preimage::PreimageKey;

/// Wire format for a set of preimage key-value pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WitnessBundle {
    /// The preimage key-value pairs.
    pub preimages: Vec<(PreimageKey, Vec<u8>)>,
}
