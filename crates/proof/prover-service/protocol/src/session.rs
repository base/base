//! Session ID derivation helpers for prover-service proof requests.

use alloy_primitives::B256;
use uuid::Uuid;

/// Shared prover-service proof session ID derivation.
#[derive(Debug)]
pub struct ProofSessionId;

impl ProofSessionId {
    /// Separator used between session ID components before `UUIDv5` hashing.
    pub const COMPONENT_SEPARATOR: &'static [u8] = b":";

    /// Derives an idempotent proof session ID from namespace, proof subtype, and root.
    pub fn derive(namespace: &[u8], proof_subtype: &str, root: B256) -> String {
        let root = root.as_slice();
        let mut name = Vec::with_capacity(
            namespace.len()
                + Self::COMPONENT_SEPARATOR.len()
                + proof_subtype.len()
                + Self::COMPONENT_SEPARATOR.len()
                + root.len(),
        );
        name.extend_from_slice(namespace);
        name.extend_from_slice(Self::COMPONENT_SEPARATOR);
        name.extend_from_slice(proof_subtype.as_bytes());
        name.extend_from_slice(Self::COMPONENT_SEPARATOR);
        name.extend_from_slice(root);

        Uuid::new_v5(&Uuid::NAMESPACE_OID, &name).to_string()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::ProofSessionId;

    #[test]
    fn derive_is_stable_for_same_inputs() {
        assert_eq!(
            ProofSessionId::derive(b"namespace", "tee/aws_nitro", B256::repeat_byte(0xaa)),
            ProofSessionId::derive(b"namespace", "tee/aws_nitro", B256::repeat_byte(0xaa)),
        );
    }

    #[test]
    fn derive_separates_namespace_subtype_and_root() {
        let root = B256::repeat_byte(0xaa);

        assert_ne!(
            ProofSessionId::derive(b"namespace", "tee/aws_nitro", root),
            ProofSessionId::derive(b"other-namespace", "tee/aws_nitro", root),
        );
        assert_ne!(
            ProofSessionId::derive(b"namespace", "tee/aws_nitro", root),
            ProofSessionId::derive(b"namespace", "zk/sp1/snark_groth16", root),
        );
        assert_ne!(
            ProofSessionId::derive(b"namespace", "tee/aws_nitro", root),
            ProofSessionId::derive(b"namespace", "tee/aws_nitro", B256::repeat_byte(0xbb)),
        );
    }
}
