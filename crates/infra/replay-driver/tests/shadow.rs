//! Multi-block replay through two real builders, with a replay-epoch clock.

use std::time::{Duration, UNIX_EPOCH};

use alloy_eips::Encodable2718;
use alloy_primitives::U256;
use alloy_provider::{Provider, ProviderBuilder};
use base_builder_core::test_utils::LocalInstance;
use base_clock::{GlobalClock, OffsetClock};
use base_common_network::Base;
use base_replay_driver::{BlockDiff, BuildWait, CanonicalBlock, DiffReport, ShadowDriver};

#[tokio::test(flavor = "multi_thread")]
async fn advances_canonical_head_when_discarding_three_shadow_candidates() -> eyre::Result<()> {
    let canonical = LocalInstance::flashblocks().await?;
    let source = canonical.driver().await?;
    let mut blocks = Vec::new();
    for nonce in 0..3 {
        let tx = source.create_transaction().with_nonce(nonce).build().await;
        let block = source.build_new_block_with_txs(vec![tx.encoded_2718().into()]).await?;
        blocks.push(block.into_consensus().map_transactions(|tx| tx.inner.into_inner()));
    }
    let extra = source
        .create_transaction()
        .with_nonce(3)
        .with_to(Default::default())
        .with_max_priority_fee_per_gas(1)
        .build()
        .await;
    GlobalClock::set(OffsetClock::new(
        UNIX_EPOCH + Duration::from_secs(blocks[0].header.timestamp - 2),
    ))?;
    let shadow = LocalInstance::flashblocks().await?;
    let provider = shadow.provider().await?;
    let engine = ProviderBuilder::new_with_network::<Base>()
        .connect_ipc(shadow.auth_ipc().to_owned().into())
        .await?;
    let driver = ShadowDriver::default();
    let recorded = blocks
        .iter()
        .flat_map(|b| &b.body.transactions)
        .filter(|tx| !tx.is_deposit())
        .map(|tx| tx.tx_hash())
        .chain(std::iter::once(extra.tx_hash()))
        .collect();
    let mut diffs = Vec::new();

    for block in &blocks {
        let diverges = block.header.number == 3;
        let canonical_hashes =
            block.body.transactions.iter().map(|tx| tx.tx_hash()).collect::<Vec<_>>();
        let mut expected = canonical_hashes.clone();
        for tx in block.body.transactions.iter().filter(|tx| !tx.is_deposit()) {
            let _pending = provider.send_raw_transaction(&tx.encoded_2718()).await?;
        }
        if diverges {
            let _pending = provider.send_raw_transaction(&extra.encoded_2718()).await?;
            expected.push(extra.tx_hash());
        }
        let candidate =
            driver.replay_block(&engine, block, shadow.node_config().chain.as_ref()).await?;

        let head = shadow.driver().await?.latest().await?;
        assert_eq!(head.header.hash, block.hash_slow());
        assert_eq!(candidate.block_number, block.header.number);
        assert_eq!(candidate.transaction_hashes != canonical_hashes, diverges);
        assert_eq!(candidate.transaction_hashes, expected);
        assert_eq!(candidate.priority_fee_total, U256::from(if diverges { 21_000 } else { 0 }));
        assert!(candidate.gas_used > 0);
        assert!(candidate.build_latency > Duration::ZERO);
        assert_eq!(candidate.flashblock_count, None);
        let receipts =
            canonical.provider().await?.get_block_receipts(block.header.number.into()).await?;
        let baseline = CanonicalBlock::from_block(block, receipts.as_deref());
        let diff = BlockDiff::compare(&candidate, &baseline, Some(&recorded))?;
        assert_eq!(diff.totals.extra, usize::from(diverges));
        assert_eq!(diff.first_divergence_index, diverges.then_some(canonical_hashes.len()));
        diffs.push(diff);
        println!(
            "shadow replay: canonical_head={}, number={}, transactions={}, gas_used={}, priority_fee_total={}, divergent={}, latency={:?}",
            head.header.hash,
            candidate.block_number,
            candidate.transaction_hashes.len(),
            candidate.gas_used,
            candidate.priority_fee_total,
            diverges,
            candidate.build_latency
        );
    }

    let report = DiffReport::new(0, diffs)?;
    assert!(report.blocks[0].warmup);
    assert!(report.blocks[1..].iter().all(|b| !b.warmup));
    assert_eq!(
        (report.aggregate.missing, report.aggregate.extra, report.aggregate.common),
        (0, 1, 4)
    );
    assert_eq!(report.aggregate.recording_gap, Some(0));
    assert_eq!(report.aggregate.builder_missing, Some(0));
    assert_eq!(report.aggregate.canonical_priority_fee_wei, Some(U256::ZERO));
    assert_eq!(report.aggregate.candidate_priority_fee_wei, U256::from(21_000));
    assert_eq!(report.aggregate.flashblock_count, None);
    println!("{}\n{}", report.markdown(), serde_json::to_string_pretty(&report)?);

    let mut invalid = blocks[2].clone();
    invalid.header.parent_hash = invalid.hash_slow();
    invalid.header.number += 1;
    invalid.header.timestamp += 2;
    let driver = ShadowDriver { wait: BuildWait::For(Duration::ZERO), ..driver };
    let result = driver.replay_block(&engine, &invalid, shadow.node_config().chain.as_ref()).await;
    assert!(matches!(result, Err(base_replay_driver::ShadowError::Invalid { .. })), "{result:?}");
    Ok(())
}
