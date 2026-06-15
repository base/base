//! Benchmarks for [`BatchReader`] constructor, decompression, and decode paths.

use std::hint::black_box;

use alloy_consensus::{SignableTransaction, TxEip1559, TxEip2930, TxEip7702, TxEnvelope, TxLegacy};
use alloy_eips::{
    eip2718::Encodable2718,
    eip2930::{AccessList, AccessListItem},
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, hex};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::Authorization;
use base_common_genesis::{HardForkConfig, RollupConfig};
use base_protocol::{
    Batch, BatchReader, BatchType, Brotli, RawSpanBatch, SpanBatchTransactionData,
};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use miniz_oxide::{
    deflate::{CompressionLevel, compress_to_vec_zlib},
    inflate::{decompress_to_vec_zlib, decompress_to_vec_zlib_with_limit},
};

const BATCH_COUNTS: [usize; 2] = [1, 64];
const SYNTHETIC_SIGNATURE_HASH_TX_COUNT: usize = 1_024;
const SYNTHETIC_SIGNATURE_HASH_RICH_INPUT_LEN: usize = 1_024;
const SYNTHETIC_SIGNATURE_HASH_RICH_ACCESS_LIST_ENTRY_COUNT: usize = 4;
const SYNTHETIC_SIGNATURE_HASH_RICH_STORAGE_KEYS_PER_ENTRY: usize = 4;
const SYNTHETIC_SIGNATURE_HASH_RICH_AUTHORIZATION_COUNT: usize = 4;
const LEGACY_DIMENSION_SENSITIVITY_SHAPES: [SyntheticSignatureHashShape; 2] =
    [SyntheticSignatureHashShape::Simple, SyntheticSignatureHashShape::InputHeavy];
const ACCESS_LIST_DIMENSION_SENSITIVITY_SHAPES: [SyntheticSignatureHashShape; 4] = [
    SyntheticSignatureHashShape::Simple,
    SyntheticSignatureHashShape::InputHeavy,
    SyntheticSignatureHashShape::AccessListHeavy,
    SyntheticSignatureHashShape::InputPlusAccessList,
];
const EIP7702_DIMENSION_SENSITIVITY_SHAPES: [SyntheticSignatureHashShape; 5] = [
    SyntheticSignatureHashShape::Simple,
    SyntheticSignatureHashShape::InputHeavy,
    SyntheticSignatureHashShape::AccessListHeavy,
    SyntheticSignatureHashShape::AuthorizationHeavy,
    SyntheticSignatureHashShape::InputPlusAuthorization,
];

#[derive(Clone)]
struct CompressionFixture {
    label: &'static str,
    compressed: Bytes,
    max_rlp_bytes_per_channel: usize,
}

#[derive(Clone)]
struct SpanTransactionFixture {
    tx_data: Vec<u8>,
    nonce: u64,
    gas: u64,
    to: Option<Address>,
    signature: Signature,
    is_protected: bool,
}

#[derive(Clone)]
struct DecodedSpanTransactionFixture {
    tx: SpanBatchTransactionData,
    nonce: u64,
    gas: u64,
    to: Option<Address>,
    signature: Signature,
    is_protected: bool,
}

#[derive(Clone)]
enum TypedTransactionFixture {
    Legacy(TxLegacy),
    Eip2930(TxEip2930),
    Eip1559(TxEip1559),
    Eip7702(TxEip7702),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypedTransactionKind {
    Legacy,
    Eip2930,
    Eip1559,
    Eip7702,
}

#[derive(Clone, Copy)]
enum SyntheticSignatureHashShape {
    Simple,
    InputHeavy,
    AccessListHeavy,
    AuthorizationHeavy,
    InputPlusAccessList,
    InputPlusAuthorization,
    Rich,
}

fn decompressed_batch_fixture(batch_count: usize) -> Vec<u8> {
    let file_contents = String::from_utf8_lossy(include_bytes!("../testdata/batch.hex"));
    let file_contents = &file_contents[..file_contents.len() - 1];
    let raw = hex::decode(file_contents).expect("batch fixture must decode");
    let single_batch = decompress_to_vec_zlib(&raw).expect("batch fixture must decompress");

    let mut multi_batch = Vec::with_capacity(single_batch.len() * batch_count);
    for _ in 0..batch_count {
        multi_batch.extend_from_slice(&single_batch);
    }
    multi_batch
}

fn compressed_batch_fixture(batch_count: usize) -> (Bytes, usize) {
    let multi_batch = decompressed_batch_fixture(batch_count);
    let max_rlp_bytes_per_channel = multi_batch.len();
    let compressed = compress_to_vec_zlib(&multi_batch, CompressionLevel::BestSpeed.into()).into();

    (compressed, max_rlp_bytes_per_channel)
}

fn brotli_compressed_batch_fixture(batch_count: usize) -> (Bytes, usize) {
    let multi_batch = decompressed_batch_fixture(batch_count);
    let max_rlp_bytes_per_channel = multi_batch.len();

    let mut compressed = vec![BatchReader::CHANNEL_VERSION_BROTLI];
    let mut input = multi_batch.as_slice();
    let params = brotli::enc::BrotliEncoderParams::default();
    brotli::BrotliCompress(&mut input, &mut compressed, &params)
        .expect("batch fixture must brotli compress");

    (compressed.into(), max_rlp_bytes_per_channel)
}

fn compression_fixtures(batch_count: usize) -> [CompressionFixture; 2] {
    let (zlib_compressed, zlib_max_rlp_bytes_per_channel) = compressed_batch_fixture(batch_count);
    let (brotli_compressed, brotli_max_rlp_bytes_per_channel) =
        brotli_compressed_batch_fixture(batch_count);

    [
        CompressionFixture {
            label: "zlib",
            compressed: zlib_compressed,
            max_rlp_bytes_per_channel: zlib_max_rlp_bytes_per_channel,
        },
        CompressionFixture {
            label: "brotli",
            compressed: brotli_compressed,
            max_rlp_bytes_per_channel: brotli_max_rlp_bytes_per_channel,
        },
    ]
}

fn decode_all_batches(mut reader: BatchReader, cfg: &RollupConfig) -> usize {
    let mut batch_count = 0;
    while reader.next_batch(cfg).is_some() {
        batch_count += 1;
    }
    batch_count
}

fn decode_all_batches_from_decompressed(mut data: &[u8], cfg: &RollupConfig) -> usize {
    let mut batch_count = 0;

    while !data.is_empty() {
        let Ok(bytes) = Bytes::decode(&mut data) else {
            break;
        };
        let Ok(_) = Batch::decode(&mut bytes.as_ref(), cfg) else {
            break;
        };
        batch_count += 1;
    }

    batch_count
}

fn batch_payloads_from_decompressed(mut data: &[u8]) -> Vec<Bytes> {
    let mut batch_payloads = Vec::new();

    while !data.is_empty() {
        let bytes = Bytes::decode(&mut data).expect("decompressed fixture must decode bytes");
        batch_payloads.push(bytes);
    }

    batch_payloads
}

fn span_batch_payloads_from_decompressed(data: &[u8]) -> Vec<Bytes> {
    batch_payloads_from_decompressed(data)
        .into_iter()
        .map(|batch_payload| match batch_payload.as_ref().first().copied() {
            Some(batch_type) if batch_type == BatchType::SPAN => batch_payload.slice(1..),
            Some(batch_type) => panic!("expected span batch fixture, got batch type {batch_type}"),
            None => panic!("batch payload fixture must not be empty"),
        })
        .collect()
}

fn raw_span_batch_templates_from_decompressed(data: &[u8]) -> Vec<RawSpanBatch> {
    span_batch_payloads_from_decompressed(data)
        .into_iter()
        .map(|raw_span_payload| {
            let mut raw_span_payload = raw_span_payload.as_ref();
            RawSpanBatch::decode(&mut raw_span_payload).expect("span batch fixture must decode")
        })
        .collect()
}

fn count_rlp_wrapped_batches(mut data: &[u8]) -> usize {
    let mut batch_count = 0;

    while !data.is_empty() {
        let Ok(_) = Bytes::decode(&mut data) else {
            break;
        };
        batch_count += 1;
    }

    batch_count
}

fn decode_all_batch_payloads(batch_payloads: &[Bytes], cfg: &RollupConfig) -> usize {
    let mut batch_count = 0;

    for payload in batch_payloads {
        let Ok(_) = Batch::decode(&mut payload.as_ref(), cfg) else {
            break;
        };
        batch_count += 1;
    }

    batch_count
}

fn decode_all_raw_span_batches(raw_span_payloads: &[Bytes]) -> usize {
    let mut batch_count = 0;

    for raw_span_payload in raw_span_payloads {
        let mut raw_span_payload = raw_span_payload.as_ref();
        let raw_span_batch =
            RawSpanBatch::decode(&mut raw_span_payload).expect("span batch fixture must decode");
        black_box(raw_span_batch);
        batch_count += 1;
    }

    batch_count
}

fn decode_all_raw_span_full_txs(raw_span_batches: &[RawSpanBatch], chain_id: u64) -> usize {
    let mut tx_count = 0;

    for raw_span_batch in raw_span_batches {
        let txs = raw_span_batch
            .payload
            .txs
            .full_txs(chain_id)
            .expect("span batch fixture transactions must decode");
        tx_count += txs.len();
        black_box(txs);
    }

    tx_count
}

fn derive_all_raw_span_batches(raw_span_batches: &mut [RawSpanBatch], cfg: &RollupConfig) -> usize {
    let mut block_count = 0;

    for raw_span_batch in raw_span_batches {
        let span_batch = raw_span_batch
            .derive(cfg.block_time, cfg.genesis.l2_time, cfg.l2_chain_id.id())
            .expect("span batch fixture must derive");
        block_count += span_batch.batches.len();
        black_box(span_batch);
    }

    block_count
}

fn span_transaction_fixtures_from_raw_span_batches(
    raw_span_batches: &[RawSpanBatch],
) -> Vec<SpanTransactionFixture> {
    let total_tx_count = raw_span_batches
        .iter()
        .map(|raw_span_batch| raw_span_batch.payload.txs.total_block_tx_count as usize)
        .sum();
    let mut fixtures = Vec::with_capacity(total_tx_count);

    for raw_span_batch in raw_span_batches {
        let txs = &raw_span_batch.payload.txs;
        let mut to_idx = 0;
        let mut protected_bit_idx = 0;

        for idx in 0..txs.total_block_tx_count as usize {
            let contract_creation_bit = txs
                .contract_creation_bits
                .get_bit(idx)
                .expect("span batch fixture contract creation bit must exist");
            let to = if contract_creation_bit == 0 {
                let to = *txs.tx_tos.get(to_idx).expect("span batch fixture to address must exist");
                to_idx += 1;
                Some(to)
            } else {
                None
            };
            let tx_type = *txs.tx_types.get(idx).expect("span batch fixture tx type must exist");
            let is_protected = if tx_type.is_legacy() {
                let is_protected =
                    txs.protected_bits.get_bit(protected_bit_idx).unwrap_or_default() == 1;
                protected_bit_idx += 1;
                is_protected
            } else {
                true
            };

            fixtures.push(SpanTransactionFixture {
                tx_data: txs.tx_data[idx].clone(),
                nonce: *txs.tx_nonces.get(idx).expect("span batch fixture nonce must exist"),
                gas: *txs.tx_gases.get(idx).expect("span batch fixture gas must exist"),
                to,
                signature: *txs.tx_sigs.get(idx).expect("span batch fixture signature must exist"),
                is_protected,
            });
        }
    }

    fixtures
}

fn decode_all_span_transaction_data(span_transactions: &[SpanTransactionFixture]) -> usize {
    let mut tx_count = 0;

    for span_transaction in span_transactions {
        let mut tx_data = span_transaction.tx_data.as_slice();
        let tx = SpanBatchTransactionData::decode(&mut tx_data)
            .expect("span batch fixture transaction data must decode");
        black_box(tx);
        tx_count += 1;
    }

    tx_count
}

fn decoded_span_transaction_fixtures(
    span_transactions: &[SpanTransactionFixture],
) -> Vec<DecodedSpanTransactionFixture> {
    span_transactions
        .iter()
        .map(|span_transaction| {
            let mut tx_data = span_transaction.tx_data.as_slice();
            let tx = SpanBatchTransactionData::decode(&mut tx_data)
                .expect("span batch fixture transaction data must decode");
            DecodedSpanTransactionFixture {
                tx,
                nonce: span_transaction.nonce,
                gas: span_transaction.gas,
                to: span_transaction.to,
                signature: span_transaction.signature,
                is_protected: span_transaction.is_protected,
            }
        })
        .collect()
}

fn u128_from_u256(value: &U256) -> u128 {
    u128::from_be_bytes(
        value.to_be_bytes::<32>()[16..]
            .try_into()
            .expect("low 16 bytes of U256 must decode as u128"),
    )
}

fn build_typed_transaction(
    span_transaction: &DecodedSpanTransactionFixture,
    chain_id: u64,
) -> TypedTransactionFixture {
    match &span_transaction.tx {
        SpanBatchTransactionData::Legacy(data) => TypedTransactionFixture::Legacy(TxLegacy {
            chain_id: span_transaction.is_protected.then_some(chain_id),
            nonce: span_transaction.nonce,
            gas_price: u128_from_u256(&data.gas_price),
            gas_limit: span_transaction.gas,
            to: span_transaction.to.map_or(TxKind::Create, TxKind::Call),
            value: data.value,
            input: data.data.clone().into(),
        }),
        SpanBatchTransactionData::Eip2930(data) => TypedTransactionFixture::Eip2930(TxEip2930 {
            chain_id,
            nonce: span_transaction.nonce,
            gas_price: u128_from_u256(&data.gas_price),
            gas_limit: span_transaction.gas,
            to: span_transaction.to.map_or(TxKind::Create, TxKind::Call),
            value: data.value,
            input: data.data.clone().into(),
            access_list: data.access_list.clone(),
        }),
        SpanBatchTransactionData::Eip1559(data) => TypedTransactionFixture::Eip1559(TxEip1559 {
            chain_id,
            nonce: span_transaction.nonce,
            max_fee_per_gas: u128_from_u256(&data.max_fee_per_gas),
            max_priority_fee_per_gas: u128_from_u256(&data.max_priority_fee_per_gas),
            gas_limit: span_transaction.gas,
            to: span_transaction.to.map_or(TxKind::Create, TxKind::Call),
            value: data.value,
            input: data.data.clone().into(),
            access_list: data.access_list.clone(),
        }),
        SpanBatchTransactionData::Eip7702(data) => TypedTransactionFixture::Eip7702(TxEip7702 {
            chain_id,
            nonce: span_transaction.nonce,
            max_fee_per_gas: u128_from_u256(&data.max_fee_per_gas),
            max_priority_fee_per_gas: u128_from_u256(&data.max_priority_fee_per_gas),
            gas_limit: span_transaction.gas,
            to: span_transaction
                .to
                .expect("span batch fixture eip7702 transaction must have a to address"),
            value: data.value,
            input: data.data.clone().into(),
            access_list: data.access_list.clone(),
            authorization_list: data.authorization_list.clone(),
        }),
    }
}

fn build_all_typed_transactions(
    span_transactions: &[DecodedSpanTransactionFixture],
    chain_id: u64,
) -> usize {
    let mut tx_count = 0;

    for span_transaction in span_transactions {
        let tx = build_typed_transaction(span_transaction, chain_id);
        black_box(tx);
        tx_count += 1;
    }

    tx_count
}

fn typed_transaction_fixtures_from_decoded_span_transactions(
    span_transactions: &[DecodedSpanTransactionFixture],
    chain_id: u64,
) -> Vec<TypedTransactionFixture> {
    span_transactions
        .iter()
        .map(|span_transaction| build_typed_transaction(span_transaction, chain_id))
        .collect()
}

impl TypedTransactionFixture {
    const fn kind(&self) -> TypedTransactionKind {
        match self {
            Self::Legacy(_) => TypedTransactionKind::Legacy,
            Self::Eip2930(_) => TypedTransactionKind::Eip2930,
            Self::Eip1559(_) => TypedTransactionKind::Eip1559,
            Self::Eip7702(_) => TypedTransactionKind::Eip7702,
        }
    }

    fn signature_hash(&self) -> B256 {
        match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
        }
    }
}

impl TypedTransactionKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Eip2930 => "eip2930",
            Self::Eip1559 => "eip1559",
            Self::Eip7702 => "eip7702",
        }
    }
}

impl SyntheticSignatureHashShape {
    const fn label(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::InputHeavy => "input_heavy",
            Self::AccessListHeavy => "access_list_heavy",
            Self::AuthorizationHeavy => "authorization_heavy",
            Self::InputPlusAccessList => "input_plus_access_list",
            Self::InputPlusAuthorization => "input_plus_authorization",
            Self::Rich => "rich",
        }
    }

    const fn input_len(self) -> usize {
        match self {
            Self::Simple | Self::AccessListHeavy | Self::AuthorizationHeavy => 32,
            Self::InputHeavy
            | Self::InputPlusAccessList
            | Self::InputPlusAuthorization
            | Self::Rich => SYNTHETIC_SIGNATURE_HASH_RICH_INPUT_LEN,
        }
    }

    const fn access_list_entry_count(self) -> usize {
        match self {
            Self::AccessListHeavy | Self::InputPlusAccessList | Self::Rich => {
                SYNTHETIC_SIGNATURE_HASH_RICH_ACCESS_LIST_ENTRY_COUNT
            }
            Self::Simple
            | Self::InputHeavy
            | Self::AuthorizationHeavy
            | Self::InputPlusAuthorization => 0,
        }
    }

    const fn authorization_count(self) -> usize {
        match self {
            Self::AuthorizationHeavy | Self::InputPlusAuthorization | Self::Rich => {
                SYNTHETIC_SIGNATURE_HASH_RICH_AUTHORIZATION_COUNT
            }
            Self::Simple | Self::InputHeavy | Self::AccessListHeavy | Self::InputPlusAccessList => {
                1
            }
        }
    }
}

fn synthetic_signature_hash_input(seed: u8, shape: SyntheticSignatureHashShape) -> Bytes {
    let input_len = shape.input_len();
    let mut input = Vec::with_capacity(input_len);
    for idx in 0..input_len {
        input.push(seed.wrapping_add(idx as u8));
    }
    input.into()
}

fn synthetic_signature_hash_access_list(
    seed: u8,
    shape: SyntheticSignatureHashShape,
) -> AccessList {
    match shape.access_list_entry_count() {
        0 => AccessList::default(),
        access_list_entry_count => (0..access_list_entry_count)
            .map(|entry_idx| AccessListItem {
                address: Address::from([seed.wrapping_add(entry_idx as u8 + 1); 20]),
                storage_keys: (0..SYNTHETIC_SIGNATURE_HASH_RICH_STORAGE_KEYS_PER_ENTRY)
                    .map(|key_idx| {
                        B256::from([seed.wrapping_add((entry_idx * 16 + key_idx) as u8); 32])
                    })
                    .collect(),
            })
            .collect::<Vec<_>>()
            .into(),
    }
}

fn synthetic_signature_hash_authorization_list(
    chain_id: u64,
    to: Address,
    nonce: u64,
    shape: SyntheticSignatureHashShape,
) -> Vec<SignedAuthorization> {
    let authorization_count = shape.authorization_count();

    (0..authorization_count)
        .map(|offset| {
            Authorization {
                chain_id: U256::from(chain_id),
                address: Address::from([to.as_slice()[0].wrapping_add(offset as u8); 20]),
                nonce: nonce + offset as u64,
            }
            .into_signed(Signature::test_signature())
        })
        .collect()
}

fn synthetic_signature_hash_fixtures(
    tx_count: usize,
    chain_id: u64,
    shape: SyntheticSignatureHashShape,
) -> Vec<(TypedTransactionKind, Vec<TypedTransactionFixture>)> {
    let mut legacy = Vec::with_capacity(tx_count);
    let mut eip2930 = Vec::with_capacity(tx_count);
    let mut eip1559 = Vec::with_capacity(tx_count);
    let mut eip7702 = Vec::with_capacity(tx_count);

    for idx in 0..tx_count {
        let nonce = idx as u64;
        let seed = ((idx % u8::MAX as usize) + 1) as u8;
        let to = Address::from([seed; 20]);
        let value = U256::from(nonce + 1);
        let gas_limit = 21_000 + (nonce % 1_024);
        let input = synthetic_signature_hash_input(seed, shape);
        let access_list = synthetic_signature_hash_access_list(seed, shape);

        legacy.push(TypedTransactionFixture::Legacy(TxLegacy {
            chain_id: Some(chain_id),
            nonce,
            gas_price: 1_000_000_000u128 + idx as u128,
            gas_limit,
            to: TxKind::Call(to),
            value,
            input: input.clone(),
        }));

        eip2930.push(TypedTransactionFixture::Eip2930(TxEip2930 {
            chain_id,
            nonce,
            gas_price: 1_000_000_000u128 + idx as u128,
            gas_limit,
            to: TxKind::Call(to),
            value,
            input: input.clone(),
            access_list: access_list.clone(),
        }));

        eip1559.push(TypedTransactionFixture::Eip1559(TxEip1559 {
            chain_id,
            nonce,
            max_fee_per_gas: 3_000_000_000u128 + idx as u128,
            max_priority_fee_per_gas: 1_000_000_000u128 + idx as u128,
            gas_limit,
            to: TxKind::Call(to),
            value,
            input: input.clone(),
            access_list: access_list.clone(),
        }));

        eip7702.push(TypedTransactionFixture::Eip7702(TxEip7702 {
            chain_id,
            nonce,
            max_fee_per_gas: 3_000_000_000u128 + idx as u128,
            max_priority_fee_per_gas: 1_000_000_000u128 + idx as u128,
            gas_limit,
            to,
            value,
            input,
            access_list,
            authorization_list: synthetic_signature_hash_authorization_list(
                chain_id, to, nonce, shape,
            ),
        }));
    }

    vec![
        (TypedTransactionKind::Legacy, legacy),
        (TypedTransactionKind::Eip2930, eip2930),
        (TypedTransactionKind::Eip1559, eip1559),
        (TypedTransactionKind::Eip7702, eip7702),
    ]
}

const fn synthetic_signature_hash_dimension_sensitivity_shapes(
    kind: TypedTransactionKind,
) -> &'static [SyntheticSignatureHashShape] {
    match kind {
        TypedTransactionKind::Legacy => &LEGACY_DIMENSION_SENSITIVITY_SHAPES,
        TypedTransactionKind::Eip2930 | TypedTransactionKind::Eip1559 => {
            &ACCESS_LIST_DIMENSION_SENSITIVITY_SHAPES
        }
        TypedTransactionKind::Eip7702 => &EIP7702_DIMENSION_SENSITIVITY_SHAPES,
    }
}

fn signature_hash_all_typed_transactions(typed_transactions: &[TypedTransactionFixture]) -> usize {
    let mut tx_count = 0;

    for typed_transaction in typed_transactions {
        let signature_hash = typed_transaction.signature_hash();
        black_box(signature_hash);
        tx_count += 1;
    }

    tx_count
}

fn typed_transaction_fixtures_grouped_by_kind(
    typed_transactions: &[TypedTransactionFixture],
) -> Vec<(TypedTransactionKind, Vec<TypedTransactionFixture>)> {
    let mut legacy = Vec::new();
    let mut eip2930 = Vec::new();
    let mut eip1559 = Vec::new();
    let mut eip7702 = Vec::new();

    for typed_transaction in typed_transactions {
        match typed_transaction.kind() {
            TypedTransactionKind::Legacy => legacy.push(typed_transaction.clone()),
            TypedTransactionKind::Eip2930 => eip2930.push(typed_transaction.clone()),
            TypedTransactionKind::Eip1559 => eip1559.push(typed_transaction.clone()),
            TypedTransactionKind::Eip7702 => eip7702.push(typed_transaction.clone()),
        }
    }

    vec![
        (TypedTransactionKind::Legacy, legacy),
        (TypedTransactionKind::Eip2930, eip2930),
        (TypedTransactionKind::Eip1559, eip1559),
        (TypedTransactionKind::Eip7702, eip7702),
    ]
}

fn build_all_signed_tx_envelopes(
    span_transactions: &[DecodedSpanTransactionFixture],
    chain_id: u64,
) -> usize {
    let mut tx_count = 0;

    for span_transaction in span_transactions {
        let tx_envelope = span_transaction
            .tx
            .to_signed_tx(
                span_transaction.nonce,
                span_transaction.gas,
                span_transaction.to,
                chain_id,
                span_transaction.signature,
                span_transaction.is_protected,
            )
            .expect("span batch fixture signed transaction must build");
        black_box(tx_envelope);
        tx_count += 1;
    }

    tx_count
}

fn signed_tx_envelopes_from_decoded_span_transactions(
    span_transactions: &[DecodedSpanTransactionFixture],
    chain_id: u64,
) -> Vec<TxEnvelope> {
    span_transactions
        .iter()
        .map(|span_transaction| {
            span_transaction
                .tx
                .to_signed_tx(
                    span_transaction.nonce,
                    span_transaction.gas,
                    span_transaction.to,
                    chain_id,
                    span_transaction.signature,
                    span_transaction.is_protected,
                )
                .expect("span batch fixture signed transaction must build")
        })
        .collect()
}

fn encode_all_signed_tx_envelopes(tx_envelopes: &[TxEnvelope]) -> usize {
    let mut total_bytes = 0;

    for tx_envelope in tx_envelopes {
        let mut buf = Vec::new();
        tx_envelope.encode_2718(&mut buf);
        total_bytes += buf.len();
        black_box(buf);
    }

    total_bytes
}

fn encode_all_signed_tx_envelopes_exact_capacity(tx_envelopes: &[TxEnvelope]) -> usize {
    let mut total_bytes = 0;

    for tx_envelope in tx_envelopes {
        let mut buf = Vec::with_capacity(tx_envelope.encode_2718_len());
        tx_envelope.encode_2718(&mut buf);
        total_bytes += buf.len();
        black_box(buf);
    }

    total_bytes
}

fn bench_rollup_config(label: &'static str) -> RollupConfig {
    match label {
        "brotli" => RollupConfig {
            hardforks: HardForkConfig { fjord_time: Some(0), ..Default::default() },
            ..Default::default()
        },
        "zlib" => RollupConfig::default(),
        unsupported => panic!("unsupported compression label: {unsupported}"),
    }
}

fn bench_batch_reader_constructor(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/constructor");
    group.sample_size(20);

    for batch_count in BATCH_COUNTS {
        let (compressed, max_rlp_bytes_per_channel) = compressed_batch_fixture(batch_count);

        group.bench_with_input(
            BenchmarkId::new("baseline_vec_clone", batch_count),
            &compressed,
            |b, compressed| {
                b.iter_batched(
                    || compressed.clone(),
                    |data| {
                        black_box(BatchReader::new(
                            black_box(data).to_vec(),
                            black_box(max_rlp_bytes_per_channel),
                        ));
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("owned_bytes", batch_count),
            &compressed,
            |b, compressed| {
                b.iter_batched(
                    || compressed.clone(),
                    |data| {
                        black_box(BatchReader::new(
                            black_box(data),
                            black_box(max_rlp_bytes_per_channel),
                        ));
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_batch_reader_decompression_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/decompression_only");
    group.sample_size(20);

    for batch_count in BATCH_COUNTS {
        let [zlib_fixture, brotli_fixture] = compression_fixtures(batch_count);

        group.bench_with_input(
            BenchmarkId::new("zlib", batch_count),
            &zlib_fixture,
            |b, fixture| {
                b.iter_batched(
                    || fixture.compressed.clone(),
                    |data| {
                        black_box(
                            decompress_to_vec_zlib_with_limit(
                                black_box(data).as_ref(),
                                black_box(fixture.max_rlp_bytes_per_channel),
                            )
                            .expect("zlib fixture must decompress"),
                        );
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("brotli", batch_count),
            &brotli_fixture,
            |b, fixture| {
                b.iter_batched(
                    || fixture.compressed.clone(),
                    |data| {
                        black_box(
                            Brotli
                                .decompress(
                                    black_box(&data[1..]),
                                    black_box(fixture.max_rlp_bytes_per_channel),
                                )
                                .expect("brotli fixture must decompress"),
                        );
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_batch_reader_decode_all_batches(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/decode_all_batches");
    group.sample_size(20);

    for batch_count in BATCH_COUNTS {
        for fixture in compression_fixtures(batch_count) {
            let cfg = bench_rollup_config(fixture.label);
            group.bench_with_input(
                BenchmarkId::new(format!("baseline_vec_clone_{}", fixture.label), batch_count),
                &fixture,
                |b, fixture| {
                    b.iter_batched(
                        || fixture.compressed.clone(),
                        |data| {
                            black_box(decode_all_batches(
                                BatchReader::new(
                                    black_box(data).to_vec(),
                                    black_box(fixture.max_rlp_bytes_per_channel),
                                ),
                                black_box(&cfg),
                            ));
                        },
                        BatchSize::SmallInput,
                    );
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("owned_bytes_{}", fixture.label), batch_count),
                &fixture,
                |b, fixture| {
                    b.iter_batched(
                        || fixture.compressed.clone(),
                        |data| {
                            black_box(decode_all_batches(
                                BatchReader::new(
                                    black_box(data),
                                    black_box(fixture.max_rlp_bytes_per_channel),
                                ),
                                black_box(&cfg),
                            ));
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

fn bench_batch_reader_post_decompression_decode_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/post_decompression_decode_only");
    group.sample_size(20);

    for batch_count in BATCH_COUNTS {
        for fixture in compression_fixtures(batch_count) {
            let cfg = bench_rollup_config(fixture.label);
            let decompressed = decompressed_batch_fixture(batch_count);

            group.bench_with_input(
                BenchmarkId::new(fixture.label, batch_count),
                &decompressed,
                |b, decompressed| {
                    b.iter_batched(
                        || decompressed.clone(),
                        |data| {
                            black_box(decode_all_batches_from_decompressed(
                                black_box(data).as_slice(),
                                black_box(&cfg),
                            ));
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();
}

fn bench_batch_reader_post_decompression_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/post_decompression_components");
    group.sample_size(20);

    for batch_count in BATCH_COUNTS {
        for fixture in compression_fixtures(batch_count) {
            let cfg = bench_rollup_config(fixture.label);
            let decompressed = decompressed_batch_fixture(batch_count);
            let batch_payloads = batch_payloads_from_decompressed(decompressed.as_slice());

            group.bench_with_input(
                BenchmarkId::new(format!("rlp_only_{}", fixture.label), batch_count),
                &decompressed,
                |b, decompressed| {
                    b.iter_batched(
                        || decompressed.clone(),
                        |data| {
                            black_box(count_rlp_wrapped_batches(black_box(data).as_slice()));
                        },
                        BatchSize::SmallInput,
                    );
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("batch_decode_only_{}", fixture.label), batch_count),
                &batch_payloads,
                |b, batch_payloads| {
                    b.iter(|| {
                        black_box(decode_all_batch_payloads(
                            black_box(batch_payloads.as_slice()),
                            black_box(&cfg),
                        ));
                    });
                },
            );
        }
    }

    group.finish();
}

fn bench_batch_reader_batch_decode_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/batch_decode_components");
    group.sample_size(20);

    let cfg = RollupConfig::default();
    let chain_id = cfg.l2_chain_id.id();

    for batch_count in BATCH_COUNTS {
        let decompressed = decompressed_batch_fixture(batch_count);
        let raw_span_payloads = span_batch_payloads_from_decompressed(decompressed.as_slice());
        let raw_span_batches = raw_span_batch_templates_from_decompressed(decompressed.as_slice());

        group.bench_with_input(
            BenchmarkId::new("raw_span_decode_only", batch_count),
            &raw_span_payloads,
            |b, raw_span_payloads| {
                b.iter(|| {
                    black_box(decode_all_raw_span_batches(black_box(raw_span_payloads.as_slice())));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_full_txs_only", batch_count),
            &raw_span_batches,
            |b, raw_span_batches| {
                b.iter_batched(
                    || raw_span_batches.clone(),
                    |raw_span_batches| {
                        black_box(decode_all_raw_span_full_txs(
                            black_box(raw_span_batches.as_slice()),
                            black_box(chain_id),
                        ));
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_derive_only", batch_count),
            &raw_span_batches,
            |b, raw_span_batches| {
                b.iter_batched(
                    || raw_span_batches.clone(),
                    |mut raw_span_batches| {
                        black_box(derive_all_raw_span_batches(
                            black_box(raw_span_batches.as_mut_slice()),
                            black_box(&cfg),
                        ));
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_batch_reader_span_full_txs_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/span_full_txs_components");
    group.sample_size(20);

    let cfg = RollupConfig::default();
    let chain_id = cfg.l2_chain_id.id();

    for batch_count in BATCH_COUNTS {
        let decompressed = decompressed_batch_fixture(batch_count);
        let raw_span_batches = raw_span_batch_templates_from_decompressed(decompressed.as_slice());
        let span_transactions = span_transaction_fixtures_from_raw_span_batches(&raw_span_batches);
        let decoded_span_transactions = decoded_span_transaction_fixtures(&span_transactions);
        let signed_tx_envelopes = signed_tx_envelopes_from_decoded_span_transactions(
            &decoded_span_transactions,
            chain_id,
        );

        group.bench_with_input(
            BenchmarkId::new("span_tx_data_decode_only", batch_count),
            &span_transactions,
            |b, span_transactions| {
                b.iter(|| {
                    black_box(decode_all_span_transaction_data(black_box(
                        span_transactions.as_slice(),
                    )));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_to_signed_tx_only", batch_count),
            &decoded_span_transactions,
            |b, decoded_span_transactions| {
                b.iter(|| {
                    black_box(build_all_signed_tx_envelopes(
                        black_box(decoded_span_transactions.as_slice()),
                        black_box(chain_id),
                    ));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_encode_2718_only", batch_count),
            &signed_tx_envelopes,
            |b, signed_tx_envelopes| {
                b.iter(|| {
                    black_box(encode_all_signed_tx_envelopes(black_box(
                        signed_tx_envelopes.as_slice(),
                    )));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_encode_2718_exact_capacity_only", batch_count),
            &signed_tx_envelopes,
            |b, signed_tx_envelopes| {
                b.iter(|| {
                    black_box(encode_all_signed_tx_envelopes_exact_capacity(black_box(
                        signed_tx_envelopes.as_slice(),
                    )));
                });
            },
        );
    }

    group.finish();
}

fn bench_batch_reader_span_signed_tx_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/span_signed_tx_components");
    group.sample_size(20);

    let cfg = RollupConfig::default();
    let chain_id = cfg.l2_chain_id.id();

    for batch_count in BATCH_COUNTS {
        let decompressed = decompressed_batch_fixture(batch_count);
        let raw_span_batches = raw_span_batch_templates_from_decompressed(decompressed.as_slice());
        let span_transactions = span_transaction_fixtures_from_raw_span_batches(&raw_span_batches);
        let decoded_span_transactions = decoded_span_transaction_fixtures(&span_transactions);
        let typed_transactions = typed_transaction_fixtures_from_decoded_span_transactions(
            &decoded_span_transactions,
            chain_id,
        );

        group.bench_with_input(
            BenchmarkId::new("span_to_signed_tx_only", batch_count),
            &decoded_span_transactions,
            |b, decoded_span_transactions| {
                b.iter(|| {
                    black_box(build_all_signed_tx_envelopes(
                        black_box(decoded_span_transactions.as_slice()),
                        black_box(chain_id),
                    ));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_build_typed_tx_only", batch_count),
            &decoded_span_transactions,
            |b, decoded_span_transactions| {
                b.iter(|| {
                    black_box(build_all_typed_transactions(
                        black_box(decoded_span_transactions.as_slice()),
                        black_box(chain_id),
                    ));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("span_signature_hash_only", batch_count),
            &typed_transactions,
            |b, typed_transactions| {
                b.iter(|| {
                    black_box(signature_hash_all_typed_transactions(black_box(
                        typed_transactions.as_slice(),
                    )));
                });
            },
        );
    }

    group.finish();
}

fn bench_batch_reader_span_signature_hash_by_tx_type(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/span_signature_hash_by_tx_type");
    group.sample_size(20);

    let cfg = RollupConfig::default();
    let chain_id = cfg.l2_chain_id.id();

    for batch_count in BATCH_COUNTS {
        let decompressed = decompressed_batch_fixture(batch_count);
        let raw_span_batches = raw_span_batch_templates_from_decompressed(decompressed.as_slice());
        let span_transactions = span_transaction_fixtures_from_raw_span_batches(&raw_span_batches);
        let decoded_span_transactions = decoded_span_transaction_fixtures(&span_transactions);
        let typed_transactions = typed_transaction_fixtures_from_decoded_span_transactions(
            &decoded_span_transactions,
            chain_id,
        );

        for (kind, typed_transactions) in
            typed_transaction_fixtures_grouped_by_kind(&typed_transactions)
        {
            if typed_transactions.is_empty() {
                continue;
            }

            group.bench_with_input(
                BenchmarkId::new(
                    format!("{}_{}_txs", kind.label(), typed_transactions.len()),
                    batch_count,
                ),
                &typed_transactions,
                |b, typed_transactions| {
                    b.iter(|| {
                        black_box(signature_hash_all_typed_transactions(black_box(
                            typed_transactions.as_slice(),
                        )));
                    });
                },
            );
        }
    }

    group.finish();
}

fn bench_batch_reader_synthetic_signature_hash_by_tx_type(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/batch_reader/synthetic_signature_hash_by_tx_type");
    group.sample_size(20);

    let chain_id = RollupConfig::default().l2_chain_id.id();

    for (kind, typed_transactions) in synthetic_signature_hash_fixtures(
        SYNTHETIC_SIGNATURE_HASH_TX_COUNT,
        chain_id,
        SyntheticSignatureHashShape::Simple,
    ) {
        group.bench_with_input(
            BenchmarkId::new(
                format!("{}_{}_txs", kind.label(), typed_transactions.len()),
                "synthetic",
            ),
            &typed_transactions,
            |b, typed_transactions| {
                b.iter(|| {
                    black_box(signature_hash_all_typed_transactions(black_box(
                        typed_transactions.as_slice(),
                    )));
                });
            },
        );
    }

    group.finish();
}

fn bench_batch_reader_synthetic_signature_hash_shape_sensitivity(c: &mut Criterion) {
    let mut group =
        c.benchmark_group("protocol/batch_reader/synthetic_signature_hash_shape_sensitivity");
    group.sample_size(10);

    let chain_id = RollupConfig::default().l2_chain_id.id();

    for shape in [SyntheticSignatureHashShape::Simple, SyntheticSignatureHashShape::Rich] {
        for (kind, typed_transactions) in
            synthetic_signature_hash_fixtures(SYNTHETIC_SIGNATURE_HASH_TX_COUNT, chain_id, shape)
        {
            group.bench_with_input(
                BenchmarkId::new(
                    format!("{}_{}_txs_{}", kind.label(), typed_transactions.len(), shape.label()),
                    "synthetic",
                ),
                &typed_transactions,
                |b, typed_transactions| {
                    b.iter(|| {
                        black_box(signature_hash_all_typed_transactions(black_box(
                            typed_transactions.as_slice(),
                        )));
                    });
                },
            );
        }
    }

    group.finish();
}

fn bench_batch_reader_synthetic_signature_hash_dimension_sensitivity(c: &mut Criterion) {
    let mut group =
        c.benchmark_group("protocol/batch_reader/synthetic_signature_hash_dimension_sensitivity");
    group.sample_size(10);

    let chain_id = RollupConfig::default().l2_chain_id.id();

    for kind in [
        TypedTransactionKind::Legacy,
        TypedTransactionKind::Eip2930,
        TypedTransactionKind::Eip1559,
        TypedTransactionKind::Eip7702,
    ] {
        for shape in synthetic_signature_hash_dimension_sensitivity_shapes(kind) {
            let typed_transactions = synthetic_signature_hash_fixtures(
                SYNTHETIC_SIGNATURE_HASH_TX_COUNT,
                chain_id,
                *shape,
            )
            .into_iter()
            .find_map(|(fixture_kind, typed_transactions)| {
                (fixture_kind == kind).then_some(typed_transactions)
            })
            .expect("synthetic signature hash fixtures must include every tx kind");

            group.bench_with_input(
                BenchmarkId::new(
                    format!("{}_{}_txs_{}", kind.label(), typed_transactions.len(), shape.label()),
                    "synthetic",
                ),
                &typed_transactions,
                |b, typed_transactions| {
                    b.iter(|| {
                        black_box(signature_hash_all_typed_transactions(black_box(
                            typed_transactions.as_slice(),
                        )));
                    });
                },
            );
        }
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_batch_reader_constructor,
    bench_batch_reader_decompression_only,
    bench_batch_reader_decode_all_batches,
    bench_batch_reader_post_decompression_decode_only,
    bench_batch_reader_post_decompression_components,
    bench_batch_reader_batch_decode_components,
    bench_batch_reader_span_full_txs_components,
    bench_batch_reader_span_signed_tx_components,
    bench_batch_reader_span_signature_hash_by_tx_type,
    bench_batch_reader_synthetic_signature_hash_by_tx_type,
    bench_batch_reader_synthetic_signature_hash_shape_sensitivity,
    bench_batch_reader_synthetic_signature_hash_dimension_sensitivity,
);
criterion_main!(benches);
