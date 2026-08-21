//! Local SP1 execution coverage for the shared Denim proof fixture.

use alloy_primitives::B256;
use base_proof_zk_backend::{DryRunZkProver, get_sp1_stdin};
use base_proof_zk_utils::{
    boot::BootInfoStruct,
    test_utils::{CLAIM_BLOCK, DENIM_FIXTURE_CONTENT_HASH, DenimFixture},
};
use sp1_sdk::SP1PublicValues;

const RANGE_CYCLE_LIMIT: u64 = 1_000_000_000_000;

async fn execute(fixture: &DenimFixture) -> anyhow::Result<(BootInfoStruct, SP1PublicValues)> {
    let content_hash = fixture.content_hash();
    let stdin = get_sp1_stdin(fixture.witness())?;
    let (public_values, stats) =
        DryRunZkProver::execute_range_program(stdin, RANGE_CYCLE_LIMIT).await?;

    assert_eq!(fixture.content_hash(), content_hash);
    assert!(stats.total_instruction_cycles > 0);
    let (decoded, _) =
        bincode::serde::decode_from_slice(public_values.as_slice(), bincode::config::legacy())?;
    Ok((decoded, public_values))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the real range ELF; run `just succinct test-denim-range`"]
async fn executes_denim_fixture_and_commits_native_vectors() {
    let fixture = DenimFixture::new();
    assert_eq!(fixture.content_hash(), DENIM_FIXTURE_CONTENT_HASH);
    let expected = fixture.expected_public_values().await;
    let (actual, public_values) = execute(&fixture).await.unwrap();

    assert_eq!(actual.l1Head, expected.l1Head);
    assert_eq!(actual.l2PreRoot, expected.l2PreRoot);
    assert_eq!(actual.l2PostRoot, fixture.expected.last().unwrap().output_root);
    assert_eq!(actual.l2PostRoot, expected.l2PostRoot);
    assert_eq!(actual.l2PreBlockNumber, 1);
    assert_eq!(actual.l2BlockNumber, CLAIM_BLOCK);
    assert_eq!(actual.rollupConfigHash, expected.rollupConfigHash);
    assert_eq!(actual.scheduleId, fixture.schedule_id);
    assert_eq!(actual.scheduleId, expected.scheduleId);
    assert_eq!(actual.intermediateRoots, expected.intermediateRoots);

    let mut expected_public_values = SP1PublicValues::new();
    expected_public_values.write(&expected);
    assert_eq!(public_values.as_slice(), expected_public_values.as_slice());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the real range ELF; run `just succinct test-denim-range`"]
async fn rejects_or_fails_to_commit_wrong_claim_and_schedule() {
    let wrong_claim = DenimFixture::new().with_claimed_output_root(B256::repeat_byte(0xaa));
    assert!(execute(&wrong_claim).await.is_err());

    let wrong_schedule = DenimFixture::new().with_schedule_block(CLAIM_BLOCK - 1);
    assert!(execute(&wrong_schedule).await.is_err());
}
