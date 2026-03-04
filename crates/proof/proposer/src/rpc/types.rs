use alloy_primitives::Bytes;
use serde::{Deserialize, Deserializer, Serialize};

/// Deserializes a vector while treating JSON `null` as an empty vector.
fn deserialize_null_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Execution witness format returned by geth/reth `debug_executionWitness`.
///
/// Both geth and reth return arrays (not maps) for codes and state.
/// This needs to be converted to the standard [`ExecutionWitness`] format.
/// The `headers` field preserves the block headers included by the node,
/// which are needed for BLOCKHASH opcode support in the enclave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RethExecutionWitness {
    /// Block headers needed for BLOCKHASH opcode support.
    /// Geth includes exactly the headers referenced by this block.
    /// These are in RPC format (camelCase, with `hash` field).
    #[serde(default)]
    pub headers: Vec<alloy_rpc_types_eth::Header>,
    /// State trie node preimages.
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    pub state: Vec<Bytes>,
    /// Contract bytecodes.
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    pub codes: Vec<Bytes>,
    /// MPT keys/paths.
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    pub keys: Vec<Bytes>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reth_witness_deserialize() {
        let json = r#"{
            "state": ["0x1234", "0x5678"],
            "codes": ["0xabcd"],
            "keys": []
        }"#;

        let witness: RethExecutionWitness = serde_json::from_str(json).unwrap();
        assert_eq!(witness.state.len(), 2);
        assert_eq!(witness.codes.len(), 1);
        assert!(witness.keys.is_empty());
    }

    #[test]
    fn test_reth_witness_deserialize_with_null_arrays() {
        let json = r#"{
            "state": null,
            "codes": ["0xabcd"],
            "keys": null
        }"#;

        let witness: RethExecutionWitness = serde_json::from_str(json).unwrap();
        assert!(witness.state.is_empty());
        assert_eq!(witness.codes.len(), 1);
        assert!(witness.keys.is_empty());
    }
}
