use hokulea_proof::eigenda_witness::EigenDAWitness;
use op_succinct_client_utils::witness::{
    preimage_store::PreimageStore, BlobData, EigenDAWitnessData,
};
use op_succinct_eigenda_host_utils::witness_generator::EigenDAWitnessGenerator;
use op_succinct_host_utils::witness_generation::WitnessGenerator;

fn default_witness() -> EigenDAWitnessData {
    EigenDAWitnessData {
        preimage_store: PreimageStore::default(),
        blob_data: BlobData::default(),
        eigenda_data: None,
    }
}

#[test]
fn test_get_sp1_stdin_with_no_eigenda_data() {
    let generator = EigenDAWitnessGenerator {};
    assert!(generator.get_sp1_stdin(default_witness()).is_ok());
}

#[test]
fn test_get_sp1_stdin_rejects_malformed_eigenda_data() {
    let generator = EigenDAWitnessGenerator {};
    let witness = EigenDAWitnessData {
        preimage_store: PreimageStore::default(),
        blob_data: BlobData::default(),
        eigenda_data: Some(vec![0xFF, 0xFF, 0xFF, 0xFF]), // Malformed data
    };

    let err = generator.get_sp1_stdin(witness).unwrap_err();
    assert!(err.to_string().contains("Failed to deserialize EigenDA blob witness data"));
}

/// Valid EigenDAWitness with no canoe proof (canoe_proof_bytes: None).
/// This is a realistic scenario for blocks without EigenDA certs requiring validity proofs.
#[test]
fn test_get_sp1_stdin_with_eigenda_data_but_no_canoe_proof() {
    let generator = EigenDAWitnessGenerator {};

    let eigenda_witness = EigenDAWitness {
        recencies: vec![],
        validities: vec![],
        encoded_payloads: vec![],
        canoe_proof_bytes: None,
    };

    let eigenda_data = serde_cbor::to_vec(&eigenda_witness).expect("serialization should work");

    let witness = EigenDAWitnessData {
        preimage_store: PreimageStore::default(),
        blob_data: BlobData::default(),
        eigenda_data: Some(eigenda_data),
    };

    assert!(generator.get_sp1_stdin(witness).is_ok());
}

/// Adversarial case: Valid EigenDAWitness structure with invalid canoe_proof_bytes.
/// This bypasses the first deserialization but should fail at nested proof deserialization.
#[test]
fn test_get_sp1_stdin_rejects_invalid_canoe_proof_bytes() {
    let generator = EigenDAWitnessGenerator {};

    // Create a valid EigenDAWitness with garbage in canoe_proof_bytes
    let eigenda_witness = EigenDAWitness {
        recencies: vec![],
        validities: vec![],
        encoded_payloads: vec![],
        canoe_proof_bytes: Some(vec![0xFF, 0xFF, 0xFF, 0xFF]), // Invalid proof bytes
    };

    let eigenda_data = serde_cbor::to_vec(&eigenda_witness).expect("serialization should work");

    let witness = EigenDAWitnessData {
        preimage_store: PreimageStore::default(),
        blob_data: BlobData::default(),
        eigenda_data: Some(eigenda_data),
    };

    let err = generator.get_sp1_stdin(witness).unwrap_err();
    assert!(err.to_string().contains("Failed to deserialize canoe proof"));
}

/// Requires: L1_RPC, L1_BEACON_RPC, L2_RPC, L2_NODE_RPC, EIGENDA_PROXY_ADDRESS
#[cfg(feature = "integration")]
mod integration {
    use super::*;
    use std::sync::Arc;

    use alloy_eips::BlockId;
    use anyhow::Result;
    use op_succinct_eigenda_host_utils::host::EigenDAOPSuccinctHost;
    use op_succinct_host_utils::{
        fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost, setup_logger,
    };
    use sp1_core_executor::SP1ReduceProof;
    use sp1_prover::InnerSC;
    use tracing::info;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_witness_generation_e2e() -> Result<()> {
        setup_logger();

        dotenv::dotenv().ok();

        let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
        let host = EigenDAOPSuccinctHost::new(Arc::new(fetcher));

        let finalized = host.fetcher.get_l2_header(BlockId::finalized()).await?;
        let (start, end) = (finalized.number.saturating_sub(1), finalized.number);

        info!("Witness generation for blocks {} -> {}", start, end);

        let host_args = host.fetch(start, end, None, false).await?;
        assert!(host_args.eigenda_proxy_address.is_some());

        let witness_data = host.run(&host_args).await?;
        assert!(witness_data.eigenda_data.is_some(), "EigenDA data should be present");

        // Verify EigenDAWitness structure and check for canoe proof
        let eigenda_data = witness_data.eigenda_data.as_ref().unwrap();
        let eigenda_witness: EigenDAWitness =
            serde_cbor::from_slice(eigenda_data).expect("EigenDA witness should deserialize");

        info!(
            "Preimages: {}, EigenDA: {} bytes, Canoe proof present: {}",
            witness_data.preimage_store.preimage_map.len(),
            eigenda_data.len(),
            eigenda_witness.canoe_proof_bytes.is_some()
        );

        // If canoe proof is present, verify it deserializes correctly
        if let Some(ref proof_bytes) = eigenda_witness.canoe_proof_bytes {
            let _proof: SP1ReduceProof<InnerSC> = serde_cbor::from_slice(proof_bytes)
                .expect("Canoe proof should deserialize to SP1ReduceProof");
            info!("Canoe proof deserialization verified ({} bytes)", proof_bytes.len());
        }

        let stdin = host.witness_generator().get_sp1_stdin(witness_data)?;
        assert!(!stdin.buffer.is_empty());

        Ok(())
    }
}
