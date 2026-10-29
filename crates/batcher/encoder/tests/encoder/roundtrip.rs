//! Round trip through the flush path. Derivation reads back the batches of the blocks the
//! encoder was given, in order, whatever the wire shape the encoder config produces, and an
//! EIP-8130 transaction comes out byte-identical.

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, U256};
use base_batcher_encoder::{BatchPipeline, BatchSubmission, DaType, EncoderConfig};
use base_common_consensus::{
    AccountChange, BaseTxEnvelope, Call, Delegation, Eip8130Constants, Eip8130Signed, TxEip8130,
};
use rstest::rstest;

use crate::common::{BlockFixture, EncoderFixture};

/// The number of blocks every case encodes.
const BLOCK_COUNT: u64 = 4;

/// Derivation reads back one batch per encoded block, in order, whatever the DA type, frame
/// size and channel split. `blob_count` and `channel_count` pin the wire shape each case targets.
#[rstest]
#[case::blobs(EncoderConfig::default(), 1_000, 1, 1)]
#[case::calldata(
    EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
    1_000,
    0,
    1
)]
#[case::calldata_small_frames(
    EncoderConfig { da_type: DaType::Calldata, max_frame_size: 64, ..EncoderConfig::default() },
    1_000,
    0,
    1
)]
#[case::blobs_small_frames(
    EncoderConfig { max_frame_size: 64, ..EncoderConfig::default() },
    1_000,
    1,
    1
)]
#[case::channel_across_blobs_and_transactions(
    EncoderConfig { max_blobs_per_tx: 2, ..EncoderConfig::default() },
    200_000,
    7,
    1
)]
// A target of one byte closes every channel on its first block.
#[case::channels_packed_in_one_blob(
    EncoderConfig { compressed_size_target: Some(1), ..EncoderConfig::default() },
    1_000,
    1,
    BLOCK_COUNT as usize
)]
fn derivation_reads_back_the_encoded_blocks(
    #[case] config: EncoderConfig,
    #[case] payload_len: usize,
    #[case] blob_count: usize,
    #[case] channel_count: usize,
) {
    let fixture = EncoderFixture::new(config);
    let mut encoder = fixture.encoder();
    let blocks = BlockFixture::chain(BLOCK_COUNT, payload_len);
    for block in &blocks {
        encoder.add_block(block.clone()).expect("fixture blocks should chain from genesis");
    }

    let submissions = encoder.encode_and_drain().expect("blocks should encode");
    assert_eq!(submissions.iter().map(BatchSubmission::blob_count).sum::<usize>(), blob_count);

    let derived = fixture.derive(&submissions);
    assert_eq!(derived.len(), channel_count);
    assert_eq!(derived.concat(), BlockFixture::batches(&blocks));
}

/// An EIP-8130 transaction reaches derivation byte-identical to its EIP-2718 encoding. The
/// expected bytes come from the transaction itself, not from the encoder's composer, so the test
/// fails if the encoder drops or alters the transaction.
#[test]
fn derivation_reads_back_eip8130_transactions_unchanged() {
    // A configured sender with a named payer, so every optional field of the encoding is set.
    let tx = TxEip8130 {
        chain_id: 8453,
        sender: Some(Address::repeat_byte(0x11)),
        nonce_key: U256::from(7),
        nonce_sequence: 3,
        valid_after: 1_000,
        valid_before: 2_000,
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 5_000_000_000,
        gas_limit: 250_000,
        account_changes: vec![AccountChange::Delegation(Delegation {
            target: Address::repeat_byte(0x22),
        })],
        calls: vec![
            vec![Call {
                to: Address::repeat_byte(0x33),
                value: U256::from(1),
                data: Bytes::from_static(&[0xde, 0xad]),
            }],
            vec![Call {
                to: Address::repeat_byte(0x44),
                value: U256::ZERO,
                data: Bytes::from_static(&[0xbe, 0xef]),
            }],
        ],
        metadata: Bytes::from_static(b"metadata"),
        payer: Some(Address::repeat_byte(0x55)),
    };
    // Configured-actor auth is `authenticator(20) || authenticator_data`.
    let authenticator = Eip8130Constants::K1_AUTHENTICATOR;
    let sender_auth = [authenticator.as_slice(), &Signature::test_signature().as_bytes()].concat();
    let payer_auth = [authenticator.as_slice(), &[0x66; 65]].concat();
    let eip8130 =
        BaseTxEnvelope::Eip8130(Eip8130Signed::new(tx, sender_auth.into(), payer_auth.into()));

    // The fixture block's user transaction becomes the EIP-8130 one, after the L1-info deposit.
    let mut block = BlockFixture::block(B256::ZERO, 1, 0);
    block.body.transactions[1] = eip8130.clone();

    let fixture = EncoderFixture::new(EncoderConfig::default());
    let mut encoder = fixture.encoder();
    encoder.add_block(block).expect("the fixture block should chain from genesis");
    let submissions = encoder.encode_and_drain().expect("the block should encode");

    let [batch] = <[_; 1]>::try_from(fixture.derive(&submissions).concat())
        .expect("derivation should read back one batch");
    assert_eq!(batch.transactions, vec![Bytes::from(eip8130.encoded_2718())]);
}
