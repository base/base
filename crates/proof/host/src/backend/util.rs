//! Utilities for the preimage server backend.

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::keccak256;
use alloy_rlp::EMPTY_STRING_CODE;
use base_proof_mpt::ordered_trie_with_encoder;
use base_proof_preimage::{PreimageKey, PreimageKeyType};
use tokio::sync::RwLock;

use crate::{KeyValueStore, Result};

/// Constructs a merkle patricia trie from the ordered list passed and stores all encoded
/// intermediate nodes of the trie in the [`KeyValueStore`].
pub async fn store_ordered_trie<KV: KeyValueStore + ?Sized, T: AsRef<[u8]>>(
    kv: &RwLock<KV>,
    values: &[T],
) -> Result<()> {
    let mut kv_write_lock = kv.write().await;

    if values.is_empty() {
        let empty_key = PreimageKey::new(*EMPTY_ROOT_HASH, PreimageKeyType::Keccak256);
        return kv_write_lock.set(empty_key.into(), [EMPTY_STRING_CODE].into());
    }

    let mut hb = ordered_trie_with_encoder(values, |node, buf| {
        buf.put_slice(node.as_ref());
    });
    hb.root();
    let intermediates = hb.take_proof_nodes().into_inner();

    for (_, value) in intermediates {
        let value_hash = keccak256(value.as_ref());
        let key = PreimageKey::new(*value_hash, PreimageKeyType::Keccak256);

        kv_write_lock.set(key.into(), value.into())?;
    }

    Ok(())
}
