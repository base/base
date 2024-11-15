//! This example decodes raw [Frame]s and reads them into a [Channel] and into a [SingleBatch].

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{hex, Address, BlockHash, Bytes, PrimitiveSignature, U256};
use alloy_rlp::Decodable;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_protocol::{BlockInfo, Channel, Frame, SingleBatch};
use std::io::Read;

fn main() {
    // Raw frame data taken from the `encode_channel` example.
    let first_frame = hex!("d9529e42ee95431e983d7e96dc2f2f0400000000004d1b1201f82f0f6c3734f4821cd090ef3979d71a98e7e483b1dccdd525024c0ef16f425c7b4976a7acc0c94a0514b72c096d4dcc52f0b22dae193c70c86d0790a304a08152c8250031d011fe80c200");
    let second_frame = hex!("d9529e42ee95431e983d7e96dc2f2f040001000000463600004009b67bf33d17f4b6831018ad78018613b3403bc2fc6da91e8fc8a29031b3417774a33bf1f30534ea695b09eb3bf26cb553530e9fa2120e755ec5bd3a2bc75b2ee30001");

    // Decode the raw frames.
    let decoded_first = Frame::decode(&first_frame).expect("decodes frame").1;
    let decoded_second = Frame::decode(&second_frame).expect("decodes frame").1;

    // Create a channel.
    let id = decoded_first.id;
    let open_block = BlockInfo::default();
    let mut channel = Channel::new(id, open_block);

    // Add the frames to the channel.
    let l1_inclusion_block = BlockInfo::default();
    channel.add_frame(decoded_first, l1_inclusion_block).expect("adds frame");
    channel.add_frame(decoded_second, l1_inclusion_block).expect("adds frame");

    // Get the frame data from the channel.
    let frame_data = channel.frame_data().expect("some frame data");
    println!("Frame data: {}", hex::encode(&frame_data));

    // Decompress the frame data with brotli.
    let decompressed = decompress_brotli(&frame_data);
    println!("Decompressed frame data: {}", hex::encode(&decompressed));

    // Decode the single batch from the decompressed data.
    let batch = SingleBatch::decode(&mut decompressed.as_slice()).expect("batch decodes");
    assert_eq!(
        batch,
        SingleBatch {
            parent_hash: BlockHash::ZERO,
            epoch_num: 1,
            epoch_hash: BlockHash::ZERO,
            timestamp: 1,
            transactions: example_transactions(),
        }
    );

    println!("Successfully decoded frames into a SingleBatch");
}

/// Decompresses the given bytes data using the Brotli decompressor implemented
/// in the [`brotli`](https://crates.io/crates/brotli) crate.
pub fn decompress_brotli(data: &[u8]) -> Vec<u8> {
    let mut reader = brotli::Decompressor::new(data, 4096);
    let mut buf = [0u8; 4096];
    let mut written = 0;
    loop {
        match reader.read(&mut buf[..]) {
            Err(e) => {
                panic!("{}", e);
            }
            Ok(size) => {
                if size == 0 {
                    break;
                }
                written += size;
                // Re-size the buffer if needed
            }
        }
    }

    // Truncate the output buffer to the written bytes
    let mut output: Vec<u8> = buf.into();
    output.truncate(written);
    output
}

fn example_transactions() -> Vec<Bytes> {
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
    let sig = PrimitiveSignature::test_signature();
    let tx_signed = tx.into_signed(sig);
    let envelope: OpTxEnvelope = tx_signed.into();
    let encoded = envelope.encoded_2718();
    transactions.push(encoded.clone().into());
    let mut slice = encoded.as_slice();
    let decoded = OpTxEnvelope::decode_2718(&mut slice).unwrap();
    assert!(matches!(decoded, OpTxEnvelope::Eip1559(_)));

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
    let sig = PrimitiveSignature::test_signature();
    let tx_signed = tx.into_signed(sig);
    let envelope: OpTxEnvelope = tx_signed.into();
    let encoded = envelope.encoded_2718();
    transactions.push(encoded.clone().into());
    let mut slice = encoded.as_slice();
    let decoded = OpTxEnvelope::decode_2718(&mut slice).unwrap();
    assert!(matches!(decoded, OpTxEnvelope::Eip1559(_)));

    transactions
}
