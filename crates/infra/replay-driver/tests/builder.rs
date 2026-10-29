//! Round-trip canonical attributes through an independent in-process flashblocks builder.

use std::time::Duration;

use base_builder_core::test_utils::LocalInstance;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_replay_driver::CanonicalAttributes;

#[tokio::test(flavor = "multi_thread")]
async fn reconstructs_canonical_block_when_driving_real_shadow_builder() -> eyre::Result<()> {
    let canonical = LocalInstance::flashblocks().await?;
    let shadow = LocalInstance::flashblocks().await?;
    let block = canonical.driver().await?.build_new_block_with_no_tx_pool().await?;
    let canonical_hash = block.header.hash;
    let block = block.into_consensus().map_transactions(|tx| tx.inner.into_inner());
    let attrs = CanonicalAttributes::derive(&block, shadow.node_config().chain.as_ref())?;
    let engine = shadow.engine_api();

    let payload = tokio::time::timeout(Duration::from_secs(15), async {
        let fcu = engine
            .update_forkchoice(
                block.header.parent_hash,
                block.header.parent_hash,
                Some(BasePayloadBuilderAttributes::try_new(block.header.parent_hash, attrs, 3)?),
            )
            .await?;
        assert!(fcu.payload_status.is_valid(), "{fcu:?}");
        engine.get_payload(fcu.payload_id.ok_or_else(|| eyre::eyre!("missing payload ID"))?).await
    })
    .await??;

    let candidate = payload.execution_payload.payload_inner.payload_inner.payload_inner;
    assert_eq!(candidate.block_hash, canonical_hash);
    assert_eq!(candidate.gas_used, block.header.gas_used);
    println!(
        "real builder round-trip: canonical={canonical_hash}, candidate={}, gas_used={}, transactions={}",
        candidate.block_hash,
        candidate.gas_used,
        candidate.transactions.len()
    );
    Ok(())
}
