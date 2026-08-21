//! An example encoding and decoding a [`SingleBatch`].
//!
//! This example demonstrates EIP-2718 encoding a [`SingleBatch`]
//! through a [`ChannelOut`] and into individual [Frame]s.
//!
//! Notice, the raw batch is first _encoded_.
//! Once encoded, it is compressed into raw data that the channel is constructed with.
//!
//! The [`ChannelOut`] then finalizes all frames using a maximum frame size of 100.
//!
//! Finally, once [Frame]s are built from the [`ChannelOut`], they are encoded and ready
//! to be batch-submitted to the data availability layer.

#[cfg(feature = "std")]
fn main() {
    use std::sync::Arc;

    use alloy_primitives::BlockHash;
    use base_common_genesis::RollupConfig;
    use base_comp::{ChannelOut, CompressionAlgo, VariantCompressor};
    use base_protocol::{ChannelId, SingleBatch};

    // Use the example transaction
    let transactions = example_transactions();

    // Construct a basic `SingleBatch`
    let parent_hash = BlockHash::ZERO;
    let epoch_num = 1;
    let epoch_hash = BlockHash::ZERO;
    let timestamp = 1;
    let single_batch = SingleBatch { parent_hash, epoch_num, epoch_hash, timestamp, transactions };

    // Create a new channel.
    let id = ChannelId::default();
    let config = Arc::new(RollupConfig::default());
    let compressor: VariantCompressor = CompressionAlgo::Brotli10.into();
    let mut channel_out = ChannelOut::new(id, config, compressor);

    // Encode and compress the batch into the channel.
    channel_out.add_single_batch(single_batch).unwrap();

    // Finalize and output frames.
    for frame in channel_out.into_frames(100).expect("outputs frames") {
        println!("Frame: {}", alloy_primitives::hex::encode(frame.encode()));
    }

    println!("Successfully encoded Batch to frames");
}

#[cfg(feature = "std")]
fn example_transactions() -> Vec<alloy_primitives::Bytes> {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
    use alloy_eips::eip2718::{Decodable2718, Encodable2718};
    use alloy_primitives::{Address, Signature, U256};

    let mut transactions = Vec::new();

    // First Transaction in the batch.
    let tx = TxEip1559 {
        chain_id: 10u64,
        nonce: 2,
        max_fee_per_gas: 3,
        max_priority_fee_per_gas: 4,
        gas_limit: 5,
        to: Address::left_padding_from(&[6]).into(),
        value: U256::from(7_u64),
        input: vec![8].into(),
        access_list: Default::default(),
    };
    let sig = Signature::test_signature();
    let tx_signed = tx.into_signed(sig);
    let envelope: TxEnvelope = tx_signed.into();
    let encoded = envelope.encoded_2718();
    transactions.push(encoded.clone().into());
    let mut slice = encoded.as_slice();
    let decoded = TxEnvelope::decode_2718(&mut slice).unwrap();
    assert!(matches!(decoded, TxEnvelope::Eip1559(_)));

    // Second transaction in the batch.
    let tx = TxEip1559 {
        chain_id: 10u64,
        nonce: 2,
        max_fee_per_gas: 3,
        max_priority_fee_per_gas: 4,
        gas_limit: 5,
        to: Address::left_padding_from(&[7]).into(),
        value: U256::from(7_u64),
        input: vec![8].into(),
        access_list: Default::default(),
    };
    let sig = Signature::test_signature();
    let tx_signed = tx.into_signed(sig);
    let envelope: TxEnvelope = tx_signed.into();
    let encoded = envelope.encoded_2718();
    transactions.push(encoded.clone().into());
    let mut slice = encoded.as_slice();
    let decoded = TxEnvelope::decode_2718(&mut slice).unwrap();
    assert!(matches!(decoded, TxEnvelope::Eip1559(_)));

    transactions
}

#[cfg(not(feature = "std"))]
fn main() {
    /* not implemented for no_std */
}
