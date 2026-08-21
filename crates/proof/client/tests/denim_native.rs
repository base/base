//! Native replay coverage for the shared hermetic Denim fixture.

use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{B256, U256, b256};
use alloy_trie::EMPTY_ROOT_HASH;
use async_trait::async_trait;
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope};
use base_common_evm::{BaseEvmFactory, BaseTime};
use base_proof::{BootInfo, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY, L2_SCHEDULE_BLOCK_NUMBER_KEY};
use base_proof_client::{
    FaultProofBlock, FaultProofProgramError, FaultProofProgramError::InvalidClaim, Prologue,
};
use base_proof_preimage::{
    HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient,
    errors::PreimageOracleResult,
};
use base_proof_zk_utils::{
    test_utils::{
        CLAIM_BLOCK, DENIM_FIXTURE_CONTENT_HASH, DENIM_TIMESTAMP, DenimFixture, ExpectedDenimBlock,
    },
    witness::preimage_store::PreimageStore,
};
use base_protocol::{BaseTimeUpdateTx, OutputRoot};

#[derive(Debug, Clone)]
struct FixtureOracle(PreimageStore);

#[async_trait]
impl PreimageOracleClient for FixtureOracle {
    async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
        self.0.get(key).await
    }

    async fn get_exact(&self, key: PreimageKey, buffer: &mut [u8]) -> PreimageOracleResult<()> {
        let value = self.get(key).await?;
        buffer.copy_from_slice(&value);
        Ok(())
    }
}

#[async_trait]
impl HintWriterClient for FixtureOracle {
    async fn write(&self, _: &str) -> PreimageOracleResult<()> {
        Ok(())
    }
}

async fn replay(
    fixture: &DenimFixture,
) -> Result<(BootInfo, Vec<FaultProofBlock>), FaultProofProgramError> {
    let boot = BootInfo::load(&fixture.store).await?;
    let oracle = FixtureOracle(fixture.store.clone());
    let driver = Prologue::new(oracle.clone(), oracle, BaseEvmFactory::default()).load().await?;
    let (epilogue, blocks) = driver.execute_with_artifacts().await?;
    epilogue.validate().map_err(|error| *error)?;
    Ok((boot, blocks))
}

fn insert_local(store: &mut PreimageStore, key: U256, value: Vec<u8>) {
    store.preimage_map.insert(PreimageKey::new_local(key.saturating_to()), value);
}

fn encode_receipt(receipt: &BaseReceiptEnvelope) -> Vec<u8> {
    let mut encoded = Vec::new();
    receipt.encode_2718(&mut encoded);
    encoded
}

fn assert_blocks(actual: &[FaultProofBlock], expected: &[ExpectedDenimBlock]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.l2_info.block_info.number, expected.number);
        assert_eq!(actual.header.timestamp, expected.timestamp);
        assert_eq!(actual.header.hash(), expected.block_hash);
        assert_eq!(actual.transactions[1], expected.base_time_transaction);
        assert_eq!(encode_receipt(&actual.receipts[1]), expected.base_time_receipt);
        assert!(actual.receipts[1].status());
        assert_eq!(actual.header.state_root, expected.state_root);
        assert_eq!(actual.output_root, expected.output_root);
        let transaction =
            BaseTxEnvelope::decode_2718(&mut actual.transactions[1].as_ref()).unwrap();
        let metadata =
            BaseTimeUpdateTx::validate_deposit(transaction.as_deposit().unwrap(), expected.number)
                .unwrap();
        assert_eq!(metadata.timestamp_millis_part(), expected.timestamp_millis_part);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replays_denim_activation_and_rollover_deterministically() {
    let fixture = DenimFixture::new();
    fixture.store.check_preimages().unwrap();
    assert_eq!(fixture.content_hash(), DENIM_FIXTURE_CONTENT_HASH);

    let (first_boot, first) = replay(&fixture).await.unwrap();
    let (second_boot, second) = replay(&fixture).await.unwrap();

    assert_eq!(
        first_boot.schedule_id,
        b256!("3b364659563c6841d115a6d137ade3201a59bfb84f7987f67ee4a45b62f30836")
    );
    assert_eq!(first_boot.schedule_id, fixture.schedule_id);
    assert_eq!(first_boot.schedule_id, second_boot.schedule_id);
    assert_eq!(first_boot.rollup_config.upgrades.base.denim, Some(DENIM_TIMESTAMP));
    assert_eq!(first_boot.claimed_l2_block_number, CLAIM_BLOCK);
    assert_blocks(&first, &fixture.expected);
    assert_blocks(&second, &fixture.expected);
    assert_eq!(first.last().unwrap().l2_info.block_info.number, first_boot.claimed_l2_block_number);
    assert_eq!(first.last().unwrap().output_root, first_boot.claimed_l2_output_root);
}

#[tokio::test(flavor = "multi_thread")]
async fn honors_schedule_override_and_intermediate_intervals() {
    for interval in [1, 2, 10] {
        let fixture = DenimFixture::with_options(None, Some(8), interval, false);
        let (boot, blocks) = replay(&fixture).await.unwrap();
        assert_eq!(
            boot.schedule_id,
            b256!("3b364659563c6841d115a6d137ade3201a59bfb84f7987f67ee4a45b62f30836")
        );
        assert_eq!(boot.schedule_id, fixture.schedule_id);
        assert_eq!(boot.intermediate_block_interval, interval);
        assert_blocks(&blocks, &fixture.expected);

        let sampled = blocks
            .iter()
            .enumerate()
            .filter(|(index, _)| (index + 1) % interval as usize == 0)
            .map(|(_, block)| block.output_root)
            .collect::<Vec<_>>();
        let expected = fixture
            .expected
            .iter()
            .enumerate()
            .filter(|(index, _)| (index + 1) % interval as usize == 0)
            .map(|(_, block)| block.output_root)
            .collect::<Vec<_>>();
        assert_eq!(sampled, expected);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replays_from_same_second_intermediate_safe_head() {
    let fixture = DenimFixture::with_options(Some(2), None, 1, false);
    let (_, blocks) = replay(&fixture).await.unwrap();
    assert_blocks(&blocks, &fixture.expected[3..]);
    assert_eq!(blocks[0].l2_info.block_info.number, 5);
    assert_eq!(blocks[0].header.timestamp, 14);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_wrong_root_and_block_claims() {
    let fixture = DenimFixture::new();
    let mut wrong_root = fixture.clone();
    insert_local(&mut wrong_root.store, L2_CLAIM_KEY, B256::repeat_byte(0xaa).to_vec());
    assert!(matches!(replay(&wrong_root).await, Err(InvalidClaim { .. })));

    let mut malformed_base_time_claim = fixture.clone();
    let malformed_root = OutputRoot::from_parts(
        fixture.expected[4].state_root,
        EMPTY_ROOT_HASH,
        fixture.expected[5].block_hash,
    )
    .hash();
    insert_local(&mut malformed_base_time_claim.store, L2_CLAIM_KEY, malformed_root.to_vec());
    assert!(matches!(
        replay(&malformed_base_time_claim).await,
        Err(InvalidClaim { claimed, .. }) if claimed == malformed_root
    ));

    let mut wrong_block = fixture;
    insert_local(
        &mut wrong_block.store,
        L2_CLAIM_BLOCK_NUMBER_KEY,
        (CLAIM_BLOCK + 1).to_be_bytes().to_vec(),
    );
    assert!(matches!(
        replay(&wrong_block).await,
        Err(FaultProofProgramError::InvalidClaimBlock { derived: CLAIM_BLOCK, claimed: 8 })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_wrong_schedule_missing_code_and_malformed_batch() {
    let mut wrong_schedule = DenimFixture::new();
    insert_local(
        &mut wrong_schedule.store,
        L2_SCHEDULE_BLOCK_NUMBER_KEY,
        (CLAIM_BLOCK - 1).to_be_bytes().to_vec(),
    );
    assert!(replay(&wrong_schedule).await.is_err());

    let mut missing_code = DenimFixture::new();
    missing_code
        .store
        .preimage_map
        .remove(&PreimageKey::new(*BaseTime::IMPLEMENTATION_CODE_HASH, PreimageKeyType::Keccak256));
    assert!(replay(&missing_code).await.is_err());

    let malformed = DenimFixture::with_options(None, None, 1, true);
    assert!(replay(&malformed).await.is_err());
}
