use alloy_consensus::Header;
use alloy_eips::{eip2718::Encodable2718, eip4844::FIELD_ELEMENTS_PER_BLOB};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::Provider;
use alloy_rlp::Decodable;
use alloy_rpc_types::{Block, debug::ExecutionWitness};
use ark_ff::{BigInteger, PrimeField};
use base_common_consensus::Predeploys;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_providers::BlobWithCommitmentAndProof;
use base_proof::{Hint, HintType, ROOTS_OF_UNITY};
use base_proof_preimage::{PreimageKey, PreimageKeyType};
use base_protocol::{BlockInfo, OutputRoot};
use tracing::warn;

use crate::{
    HostConfig, HostError, HostProviders, Metrics, Result, SharedKeyValueStore, store_ordered_trie,
};

/// Parses a blob hint, supporting both legacy (48-byte) and new (40-byte) formats.
///
/// Returns the blob hash and timestamp.
///
/// ## Formats
/// - Legacy: hash (32 bytes) + index (8 bytes) + timestamp (8 bytes) = 48 bytes
/// - New: hash (32 bytes) + timestamp (8 bytes) = 40 bytes
///
/// The legacy index field is parsed but ignored.
pub fn parse_blob_hint(hint_data: &[u8]) -> Result<(B256, u64)> {
    match hint_data.len() {
        48 => {
            let hash_data_bytes: [u8; 32] = hint_data[0..32].try_into()?;
            let _index_data_bytes: [u8; 8] = hint_data[32..40].try_into()?;
            let timestamp_data_bytes: [u8; 8] = hint_data[40..48].try_into()?;

            let hash: B256 = hash_data_bytes.into();
            let timestamp = u64::from_be_bytes(timestamp_data_bytes);
            Ok((hash, timestamp))
        }
        40 => {
            let hash_data_bytes: [u8; 32] = hint_data[0..32].try_into()?;
            let timestamp_data_bytes: [u8; 8] = hint_data[32..40].try_into()?;

            let hash: B256 = hash_data_bytes.into();
            let timestamp = u64::from_be_bytes(timestamp_data_bytes);
            Ok((hash, timestamp))
        }
        _ => Err(HostError::Custom(format!(
            "Invalid blob hint length: expected 40 or 48 bytes, got {}",
            hint_data.len()
        ))),
    }
}

/// Fetches data in response to a hint.
pub async fn handle_hint(
    hint: Hint<HintType>,
    cfg: &HostConfig,
    providers: &HostProviders,
    kv: SharedKeyValueStore,
) -> Result<()> {
    let hint_type_label: &str = hint.ty.into();

    Metrics::hint_requests_total(hint_type_label).increment(1);
    let _timer = base_metrics::timed!(Metrics::hint_duration_seconds(hint_type_label));

    let result = Box::pin(handle_hint_inner(hint, cfg, providers, kv)).await;

    if result.is_err() {
        Metrics::hint_errors_total(hint_type_label).increment(1);
    }

    result
}

async fn handle_hint_inner(
    hint: Hint<HintType>,
    cfg: &HostConfig,
    providers: &HostProviders,
    kv: SharedKeyValueStore,
) -> Result<()> {
    match hint.ty {
        HintType::L1BlockHeader => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let raw_header: Bytes =
                providers.l1.client().request("debug_getRawHeader", [hash]).await?;

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
        }
        HintType::L1Transactions => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let Block { transactions, .. } = providers
                .l1
                .get_block_by_hash(hash)
                .full()
                .await?
                .ok_or(HostError::BlockNotFound)?;
            let encoded_transactions = transactions
                .into_transactions()
                .map(|tx| tx.inner.encoded_2718())
                .collect::<Vec<_>>();

            store_ordered_trie(kv.as_ref(), encoded_transactions.as_slice()).await?;
        }
        HintType::L1Receipts => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let raw_receipts: Vec<Bytes> =
                providers.l1.client().request("debug_getRawReceipts", [hash]).await?;

            store_ordered_trie(kv.as_ref(), raw_receipts.as_slice()).await?;
        }
        HintType::L1Blob => {
            let (hash, timestamp) = parse_blob_hint(&hint.data)?;

            let partial_block_ref = BlockInfo { timestamp, ..Default::default() };

            let mut blobs = providers
                .blobs
                .fetch_blobs_with_proofs(&partial_block_ref, &[hash])
                .await
                .map_err(|e| HostError::BlobSidecarFetchFailed(e.to_string()))?;
            if blobs.len() != 1 {
                return Err(HostError::BlobCountMismatch { expected: 1, actual: blobs.len() });
            }
            let BlobWithCommitmentAndProof { blob, kzg_proof: proof, kzg_commitment: commitment } =
                blobs.pop().expect("Expected 1 blob");

            let mut kv_lock = kv.write().await;

            kv_lock.set(
                PreimageKey::new(*hash, PreimageKeyType::Sha256).into(),
                commitment.to_vec(),
            )?;

            let mut blob_key = [0u8; 80];
            blob_key[..48].copy_from_slice(commitment.as_ref());
            for i in 0..FIELD_ELEMENTS_PER_BLOB {
                blob_key[48..].copy_from_slice(
                    ROOTS_OF_UNITY[i as usize].into_bigint().to_bytes_be().as_ref(),
                );
                let blob_key_hash = keccak256(blob_key.as_ref());

                kv_lock.set(PreimageKey::new_keccak256(*blob_key_hash).into(), blob_key.into())?;
                kv_lock.set(
                    PreimageKey::new(*blob_key_hash, PreimageKeyType::Blob).into(),
                    blob.as_ref()[(i as usize) << 5..(i as usize + 1) << 5].to_vec(),
                )?;
            }

            blob_key[72..].copy_from_slice(FIELD_ELEMENTS_PER_BLOB.to_be_bytes().as_ref());
            let blob_key_hash = keccak256(blob_key.as_ref());

            kv_lock.set(PreimageKey::new_keccak256(*blob_key_hash).into(), blob_key.into())?;
            kv_lock.set(
                PreimageKey::new(*blob_key_hash, PreimageKeyType::Blob).into(),
                proof.to_vec(),
            )?;
        }
        HintType::L1Precompile => {
            if hint.data.len() < 28 {
                return Err(HostError::InvalidHintDataLength);
            }

            let input_hash = keccak256(hint.data.as_ref());

            #[cfg(feature = "precompiles")]
            let result = {
                let address = Address::from_slice(&hint.data.as_ref()[..20]);
                let gas = u64::from_be_bytes(hint.data.as_ref()[20..28].try_into()?);
                let input = hint.data[28..].to_vec();
                crate::precompiles::execute(address, input, gas).map_or_else(
                    |_| vec![0u8; 1],
                    |raw_res: Vec<u8>| {
                        let mut res = Vec::with_capacity(1 + raw_res.len());
                        res.push(0x01);
                        res.extend_from_slice(&raw_res);
                        res
                    },
                )
            };
            #[cfg(not(feature = "precompiles"))]
            let result = vec![0u8; 1];

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*input_hash).into(), hint.data.into())?;
            kv_lock
                .set(PreimageKey::new(*input_hash, PreimageKeyType::Precompile).into(), result)?;
        }
        HintType::L2BlockHeader => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let raw_header: Bytes =
                providers.l2.client().request("debug_getRawHeader", [hash]).await?;

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*hash).into(), raw_header.into())?;
        }
        HintType::L2Transactions => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;
            let Block { transactions, .. } = providers
                .l2
                .get_block_by_hash(hash)
                .full()
                .await?
                .ok_or(HostError::BlockNotFound)?;

            let encoded_transactions = transactions
                .into_transactions()
                .map(|tx| tx.inner.inner.encoded_2718())
                .collect::<Vec<_>>();
            store_ordered_trie(kv.as_ref(), encoded_transactions.as_slice()).await?;
        }
        HintType::StartingL2Output => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let raw_header: Bytes = providers
                .l2
                .client()
                .request("debug_getRawHeader", &[cfg.request.agreed_l2_head_hash])
                .await?;
            let header = Header::decode(&mut raw_header.as_ref())?;

            let l2_to_l1_message_passer = providers
                .l2
                .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Default::default())
                .block_id(cfg.request.agreed_l2_head_hash.into())
                .await?;

            let output_root = OutputRoot::from_parts(
                header.state_root,
                l2_to_l1_message_passer.storage_hash,
                cfg.request.agreed_l2_head_hash,
            );
            let output_root_hash = output_root.hash();

            if output_root_hash != cfg.request.agreed_l2_output_root {
                return Err(HostError::OutputRootMismatch);
            }

            let mut kv_write_lock = kv.write().await;
            kv_write_lock.set(
                PreimageKey::new_keccak256(*output_root_hash).into(),
                output_root.encode().into(),
            )?;
        }
        HintType::L2Code => {
            const CODE_PREFIX: u8 = b'c';

            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;

            let code_key = [&[CODE_PREFIX], hash.as_slice()].concat();
            let code = providers
                .l2
                .client()
                .request::<&[Bytes; 1], Bytes>("debug_dbGet", &[code_key.into()])
                .await;

            let code = match code {
                Ok(code) => code,
                Err(_) => providers
                    .l2
                    .client()
                    .request::<&[B256; 1], Bytes>("debug_dbGet", &[hash])
                    .await
                    .map_err(|e| HostError::CodeHashPreimageFetchFailed(e.to_string()))?,
            };

            let mut kv_lock = kv.write().await;
            kv_lock.set(PreimageKey::new_keccak256(*hash).into(), code.into())?;
        }
        HintType::L2StateNode => {
            if hint.data.len() != 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let hash: B256 = hint.data.as_ref().try_into()?;

            warn!(node_hash = %hash, "L2StateNode hint sent");
            warn!("debug_executePayload failed to return a complete witness");

            let preimage: Bytes = providers.l2.client().request("debug_dbGet", &[hash]).await?;

            let mut kv_write_lock = kv.write().await;
            kv_write_lock.set(PreimageKey::new_keccak256(*hash).into(), preimage.into())?;
        }
        HintType::L2AccountProof => {
            if hint.data.len() != 8 + 20 {
                return Err(HostError::InvalidHintDataLength);
            }

            let block_number = u64::from_be_bytes(hint.data.as_ref()[..8].try_into()?);
            let address = Address::from_slice(&hint.data.as_ref()[8..28]);

            let proof_response = providers
                .l2
                .get_proof(address, Default::default())
                .block_id(block_number.into())
                .await?;

            let mut kv_lock = kv.write().await;
            proof_response.account_proof.into_iter().try_for_each(|node| {
                let node_hash = keccak256(node.as_ref());
                let key = PreimageKey::new_keccak256(*node_hash);
                kv_lock.set(key.into(), node.into())?;
                Ok::<(), HostError>(())
            })?;
        }
        HintType::L2AccountStorageProof => {
            if hint.data.len() != 8 + 20 + 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let block_number = u64::from_be_bytes(hint.data.as_ref()[..8].try_into()?);
            let address = Address::from_slice(&hint.data.as_ref()[8..28]);
            let slot = B256::from_slice(&hint.data.as_ref()[28..]);

            let proof_response =
                providers.l2.get_proof(address, vec![slot]).block_id(block_number.into()).await?;

            let mut kv_lock = kv.write().await;

            proof_response.account_proof.into_iter().try_for_each(|node| {
                let node_hash = keccak256(node.as_ref());
                let key = PreimageKey::new_keccak256(*node_hash);
                kv_lock.set(key.into(), node.into())?;
                Ok::<(), HostError>(())
            })?;

            let storage_proof = proof_response
                .storage_proof
                .into_iter()
                .next()
                .ok_or_else(|| HostError::Custom("empty storage proof from RPC".into()))?;
            storage_proof.proof.into_iter().try_for_each(|node| {
                let node_hash = keccak256(node.as_ref());
                let key = PreimageKey::new_keccak256(*node_hash);
                kv_lock.set(key.into(), node.into())?;
                Ok::<(), HostError>(())
            })?;
        }
        HintType::L2PayloadWitness => {
            if !cfg.prover.enable_experimental_witness_endpoint {
                warn!("L2PayloadWitness hint sent but payload witness is disabled, skipping");
                return Ok(());
            }

            if hint.data.len() < 32 {
                return Err(HostError::InvalidHintDataLength);
            }

            let parent_block_hash = B256::from_slice(&hint.data.as_ref()[..32]);
            let payload_attributes: BasePayloadAttributes =
                serde_json::from_slice(&hint.data[32..])?;

            let execute_payload_response = match providers
                .l2
                .client()
                .request::<(B256, BasePayloadAttributes), ExecutionWitness>(
                    "debug_executePayload",
                    (parent_block_hash, payload_attributes),
                )
                .await
            {
                Ok(response) => response,
                Err(e) => {
                    warn!(error = %e, "debug_executePayload failed");
                    return Ok(());
                }
            };

            let preimages = execute_payload_response
                .state
                .into_iter()
                .chain(execute_payload_response.codes)
                .chain(execute_payload_response.keys);

            let mut kv_lock = kv.write().await;
            for preimage in preimages {
                let preimage_bytes: Vec<u8> = preimage.into();
                let computed_hash = keccak256(&preimage_bytes);

                let key = PreimageKey::new_keccak256(*computed_hash);
                kv_lock.set(key.into(), preimage_bytes)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HASH: B256 = B256::new([0x42u8; 32]);
    const TEST_TIMESTAMP: u64 = 1234567890;

    const LEGACY_HINT: [u8; 48] = [
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFA, 0xCA, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x96, 0x02, 0xD2,
    ];

    const NEW_HINT: [u8; 40] = [
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
        0x42, 0x42, 0x00, 0x00, 0x00, 0x00, 0x49, 0x96, 0x02, 0xD2,
    ];

    #[test]
    fn test_parse_blob_hint_formats() {
        let (legacy_hash, legacy_timestamp) = parse_blob_hint(&LEGACY_HINT).unwrap();
        let (new_hash, new_timestamp) = parse_blob_hint(&NEW_HINT).unwrap();

        assert_eq!(legacy_hash, TEST_HASH);
        assert_eq!(legacy_timestamp, TEST_TIMESTAMP);
        assert_eq!(new_hash, TEST_HASH);
        assert_eq!(new_timestamp, TEST_TIMESTAMP);
    }

    #[test]
    fn test_parse_blob_hint_invalid_length() {
        let hint_data = vec![0u8; 35];
        let result = parse_blob_hint(&hint_data);

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Invalid blob hint length"));
        assert!(err_msg.contains("expected 40 or 48 bytes"));
        assert!(err_msg.contains("got 35"));
    }
}
