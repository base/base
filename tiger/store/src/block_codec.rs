//! base-reth's `Compact` encodings of Base transactions and receipts, as stored in the
//! transaction and receipt static files.
//!
//! Hand-written after reth-codecs 0.8.2 (`alloy/transaction/*`, `alloy/signature.rs`,
//! `alloy/access_list.rs`, `alloy/authorization_list.rs`, `alloy/log.rs`, the `Compact` and
//! `CompactZstd` derives) and base/base `crates/common/consensus/src/reth_compat.rs`.
//!
//! - A transaction is a flags byte (signature parity bit, 2-bit type identifier, zstd bit), the
//!   signature (`r` and `s` little-endian), then the type's extension byte (EIP-7702, deposit) and
//!   the transaction fields. With 32 or more bytes of input, the extension byte and fields are
//!   zstd-compressed with reth's transaction dictionary.
//! - A receipt is a 2-byte flags header, then the extension byte, cumulative gas, logs, deposit
//!   fields and committed phases; a body longer than 7 bytes is zstd-compressed with reth's
//!   receipt dictionary.
//!
//! Base's native transaction type keeps its compact head, an RLP tail of account changes, calls
//! and metadata, and an RLP list of the two authentication payloads.

use std::cell::RefCell;

use alloy_consensus::{Receipt, Sealed, Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
use alloy_eips::{
    eip2930::{AccessList, AccessListItem},
    eip7702::{Authorization, SignedAuthorization},
};
use alloy_primitives::{Address, B256, Bytes, Log, LogData, Signature, U256};
use alloy_rlp::{Decodable, Encodable};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope, DepositReceipt, TxDeposit};

use crate::{
    CodecError, Decoded, Reader, put_compact_u64, put_varuint,
    codec::{F, W},
};

static TRANSACTION_DICTIONARY: &[u8] = include_bytes!("../zstd/transaction_dictionary.bin");
static RECEIPT_DICTIONARY: &[u8] = include_bytes!("../zstd/receipt_dictionary.bin");

thread_local! {
    static TX_COMPRESSOR: RefCell<zstd::bulk::Compressor<'static>> =
        RefCell::new(zstd::bulk::Compressor::with_dictionary(0, TRANSACTION_DICTIONARY).expect("tx dictionary"));
    static TX_DECOMPRESSOR: RefCell<zstd::bulk::Decompressor<'static>> =
        RefCell::new(zstd::bulk::Decompressor::with_dictionary(TRANSACTION_DICTIONARY).expect("tx dictionary"));
    static RECEIPT_COMPRESSOR: RefCell<zstd::bulk::Compressor<'static>> =
        RefCell::new(zstd::bulk::Compressor::with_dictionary(0, RECEIPT_DICTIONARY).expect("receipt dictionary"));
    static RECEIPT_DECOMPRESSOR: RefCell<zstd::bulk::Decompressor<'static>> =
        RefCell::new(zstd::bulk::Decompressor::with_dictionary(RECEIPT_DICTIONARY).expect("receipt dictionary"));
}

/// Largest decompressed transaction or receipt body accepted.
const MAX_DECOMPRESSED: usize = 64 << 20;

fn decompress(
    cell: &'static std::thread::LocalKey<RefCell<zstd::bulk::Decompressor<'static>>>,
    src: &[u8],
) -> Decoded<Vec<u8>> {
    let size = zstd::zstd_safe::get_frame_content_size(src).ok().flatten().unwrap_or(MAX_DECOMPRESSED as u64);
    if size > MAX_DECOMPRESSED as u64 {
        return Err(CodecError("zstd frame too large"));
    }
    cell.with_borrow_mut(|d| d.decompress(src, size as usize)).map_err(|_| CodecError("bad zstd frame"))
}

const IDENTIFIER_LEGACY: u64 = 0;
const IDENTIFIER_EIP2930: u64 = 1;
const IDENTIFIER_EIP1559: u64 = 2;
const IDENTIFIER_EXTENDED: u64 = 3;
const EIP7702_TYPE: u8 = 4;
const DEPOSIT_TYPE: u8 = 0x7e;

fn put_access_list(out: &mut Vec<u8>, list: &AccessList) {
    put_varuint(out, list.0.len());
    for item in &list.0 {
        let mut tmp = Vec::with_capacity(21 + 32 * item.storage_keys.len());
        tmp.extend_from_slice(item.address.as_slice());
        put_varuint(&mut tmp, item.storage_keys.len());
        for key in &item.storage_keys {
            tmp.extend_from_slice(key.as_slice());
        }
        put_varuint(out, tmp.len());
        out.extend_from_slice(&tmp);
    }
}

fn get_access_list(r: &mut Reader<'_>) -> Decoded<AccessList> {
    let count = r.varuint()?;
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let len = r.varuint()?;
        let mut item = Reader::new(r.take(len)?);
        let address = item.address()?;
        let keys = item.varuint()?;
        let mut storage_keys = Vec::with_capacity(keys.min(1024));
        for _ in 0..keys {
            storage_keys.push(item.b256()?);
        }
        items.push(AccessListItem { address, storage_keys });
    }
    Ok(AccessList(items))
}

fn put_authorizations(out: &mut Vec<u8>, list: &[SignedAuthorization]) {
    put_varuint(out, list.len());
    for auth in list {
        let mut tmp = Vec::with_capacity(100);
        tmp.push(auth.y_parity());
        tmp.extend_from_slice(&auth.r().to_le_bytes::<32>());
        tmp.extend_from_slice(&auth.s().to_le_bytes::<32>());
        let mut w = W::default();
        w.u256(auth.chain_id);
        w.raw(auth.address.as_slice());
        w.u64(auth.nonce);
        tmp.extend_from_slice(&w.finish());
        put_varuint(out, tmp.len());
        out.extend_from_slice(&tmp);
    }
}

fn get_authorizations(r: &mut Reader<'_>) -> Decoded<Vec<SignedAuthorization>> {
    let count = r.varuint()?;
    let mut list = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let len = r.varuint()?;
        let mut a = Reader::new(r.take(len)?);
        let y_parity = a.u8()?;
        let r_ = U256::from_le_slice(a.take(32)?);
        let s = U256::from_le_slice(a.take(32)?);
        let mut f = a.flags(2)?;
        let chain_id = a.compact_u256(f.get(6) as usize)?;
        let address = a.address()?;
        let nonce = a.compact_u64(f.get(4) as usize)?;
        list.push(SignedAuthorization::new_unchecked(Authorization { chain_id, address, nonce }, y_parity, r_, s));
    }
    Ok(list)
}

static EMPTY_INPUT: Bytes = Bytes::new();

/// A Base transaction's compact tail: `rlp([rlp(account_changes), rlp(calls), metadata])` with
/// the first two as byte strings.
fn base_tail(t: &base_common_consensus::TxBase) -> Vec<u8> {
    let account_changes = Bytes::from(alloy_rlp::encode(&t.account_changes));
    let payload: usize = t.calls.iter().map(Encodable::length).sum();
    let mut calls = Vec::with_capacity(payload + 9);
    alloy_rlp::Header { list: true, payload_length: payload }.encode(&mut calls);
    for phase in &t.calls {
        phase.encode(&mut calls);
    }
    let calls = Bytes::from(calls);
    let payload = account_changes.length() + calls.length() + t.metadata.length();
    let mut tail = Vec::with_capacity(payload + 9);
    alloy_rlp::Header { list: true, payload_length: payload }.encode(&mut tail);
    account_changes.encode(&mut tail);
    calls.encode(&mut tail);
    t.metadata.encode(&mut tail);
    tail
}

type BaseTail = (Vec<base_common_consensus::AccountChange>, Vec<Vec<base_common_consensus::Call>>, Bytes);

fn decode_base_tail(mut tail: &[u8]) -> Decoded<BaseTail> {
    let bad = |_| CodecError("bad Base transaction tail");
    let header = alloy_rlp::Header::decode(&mut tail).map_err(bad)?;
    let before = tail.len();
    let account_changes = Bytes::decode(&mut tail).map_err(bad)?;
    let calls = Bytes::decode(&mut tail).map_err(bad)?;
    let metadata = Bytes::decode(&mut tail).map_err(bad)?;
    if !header.list || before - tail.len() != header.payload_length || !tail.is_empty() {
        return Err(CodecError("bad Base transaction tail list"));
    }
    let mut buf = account_changes.as_ref();
    let account_changes = Vec::<base_common_consensus::AccountChange>::decode(&mut buf).map_err(bad)?;
    let mut calls_buf = calls.as_ref();
    let calls = base_common_consensus::TxBase::decode_calls(&mut calls_buf).map_err(bad)?;
    if !buf.is_empty() || !calls_buf.is_empty() {
        return Err(CodecError("trailing bytes in Base transaction tail"));
    }
    Ok((account_changes, calls, metadata))
}

/// Transaction static-file rows.
#[derive(Debug)]
pub struct TxCodec;

impl TxCodec {
    fn fields(tx: &BaseTxEnvelope, out: &mut Vec<u8>) -> Result<u64, CodecError> {
        let mut w = W::default();
        let identifier = match tx {
            BaseTxEnvelope::Legacy(s) => {
                let t = s.tx();
                w.opt(t.chain_id.map(u128::from));
                w.u64(t.nonce);
                w.u128(t.gas_price);
                w.u64(t.gas_limit);
                w.kind(t.to);
                w.u256(t.value);
                w.raw(&t.input);
                IDENTIFIER_LEGACY
            }
            BaseTxEnvelope::Eip2930(s) => {
                let t = s.tx();
                w.u64(t.chain_id);
                w.u64(t.nonce);
                w.u128(t.gas_price);
                w.u64(t.gas_limit);
                w.kind(t.to);
                w.u256(t.value);
                put_access_list(&mut w.body, &t.access_list);
                w.raw(&t.input);
                IDENTIFIER_EIP2930
            }
            BaseTxEnvelope::Eip1559(s) => {
                let t = s.tx();
                w.u64(t.chain_id);
                w.u64(t.nonce);
                w.u64(t.gas_limit);
                w.u128(t.max_fee_per_gas);
                w.u128(t.max_priority_fee_per_gas);
                w.kind(t.to);
                w.u256(t.value);
                put_access_list(&mut w.body, &t.access_list);
                w.raw(&t.input);
                IDENTIFIER_EIP1559
            }
            BaseTxEnvelope::Eip7702(s) => {
                let t = s.tx();
                out.push(EIP7702_TYPE);
                w.u64(t.chain_id);
                w.u64(t.nonce);
                w.u64(t.gas_limit);
                w.u128(t.max_fee_per_gas);
                w.u128(t.max_priority_fee_per_gas);
                w.raw(t.to.as_slice());
                w.u256(t.value);
                put_access_list(&mut w.body, &t.access_list);
                put_authorizations(&mut w.body, &t.authorization_list);
                w.raw(&t.input);
                IDENTIFIER_EXTENDED
            }
            BaseTxEnvelope::Deposit(d) => {
                out.push(DEPOSIT_TYPE);
                w.raw(d.source_hash.as_slice());
                w.raw(d.from.as_slice());
                w.kind(d.to);
                w.opt((d.mint != 0).then_some(d.mint));
                w.u256(d.value);
                w.u64(d.gas_limit);
                w.bool(d.is_system_transaction);
                w.raw(&d.input);
                IDENTIFIER_EXTENDED
            }
            BaseTxEnvelope::Base(signed) => {
                out.push(base_common_consensus::BASE_TX_TYPE_ID);
                let t = signed.tx();
                let tail = base_tail(t);
                w.u64(t.chain_id);
                w.opt_raw(t.sender.as_ref().map(|a| a.as_slice()));
                w.u256(t.nonce_key);
                w.u64(t.nonce_sequence);
                w.u64(t.valid_after);
                w.u64(t.valid_before);
                w.u128(t.max_priority_fee_per_gas);
                w.u128(t.max_fee_per_gas);
                w.u64(t.gas_limit);
                w.opt_raw(t.payer.as_ref().map(|a| a.as_slice()));
                w.u64(tail.len() as u64);
                out.extend_from_slice(&w.finish());
                out.extend_from_slice(&tail);
                let auth_len = signed.sender_auth().length() + signed.payer_auth().length();
                alloy_rlp::Header { list: true, payload_length: auth_len }.encode(out);
                signed.sender_auth().encode(out);
                signed.payer_auth().encode(out);
                return Ok(IDENTIFIER_EXTENDED);
            }
        };
        out.extend_from_slice(&w.finish());
        Ok(identifier)
    }

    /// Encodes `tx` as base-reth stores it.
    pub fn encode(tx: &BaseTxEnvelope) -> Result<Vec<u8>, CodecError> {
        let zero = Signature::new(U256::ZERO, U256::ZERO, false);
        let (signature, input) = match tx {
            BaseTxEnvelope::Legacy(s) => (*s.signature(), &s.tx().input),
            BaseTxEnvelope::Eip2930(s) => (*s.signature(), &s.tx().input),
            BaseTxEnvelope::Eip1559(s) => (*s.signature(), &s.tx().input),
            BaseTxEnvelope::Eip7702(s) => (*s.signature(), &s.tx().input),
            BaseTxEnvelope::Deposit(d) => (zero, &d.input),
            // Base transactions carry their authentication inside; their input is empty.
            BaseTxEnvelope::Base(_) => (zero, &EMPTY_INPUT),
        };
        let mut out = vec![0u8];
        out.extend_from_slice(&signature.r().to_le_bytes::<32>());
        out.extend_from_slice(&signature.s().to_le_bytes::<32>());
        let zstd = input.len() >= 32;
        let mut fields = Vec::with_capacity(input.len() + 128);
        let identifier = Self::fields(tx, &mut fields)?;
        if zstd {
            let compressed = TX_COMPRESSOR.with_borrow_mut(|c| c.compress(&fields)).map_err(|_| CodecError("zstd failed"))?;
            out.extend_from_slice(&compressed);
        } else {
            out.extend_from_slice(&fields);
        }
        out[0] = u8::from(signature.v()) | ((identifier as u8) << 1) | (u8::from(zstd) << 3);
        Ok(out)
    }

    /// Decodes a stored transaction.
    pub fn decode(bytes: &[u8]) -> Decoded<BaseTxEnvelope> {
        let mut r = Reader::new(bytes);
        let flags = r.u8()?;
        let parity = flags & 1 == 1;
        let identifier = u64::from((flags >> 1) & 3);
        let rr = U256::from_le_slice(r.take(32)?);
        let s = U256::from_le_slice(r.take(32)?);
        let signature = Signature::new(rr, s, parity);
        let fields = if flags & 8 != 0 { decompress(&TX_DECOMPRESSOR, r.rest)? } else { r.rest.to_vec() };
        let mut r = Reader::new(&fields);
        let ty = if identifier == IDENTIFIER_EXTENDED { r.u8()? } else { identifier as u8 };
        let tx = match (identifier, ty) {
            (IDENTIFIER_LEGACY, _) => {
                let mut f = F { flags: r.flags(3)?, r: &mut r };
                let chain_id = f.opt()?.map(|c| c as u64);
                let nonce = f.u64()?;
                let gas_price = f.u128()?;
                let gas_limit = f.u64()?;
                let to = f.kind()?;
                let value = f.u256()?;
                let input = Bytes::copy_from_slice(f.r.rest);
                let tx = TxLegacy { chain_id, nonce, gas_price, gas_limit, to, value, input };
                BaseTxEnvelope::Legacy(Signed::new_unhashed(tx, signature))
            }
            (IDENTIFIER_EIP2930, _) => {
                let mut f = F { flags: r.flags(3)?, r: &mut r };
                let chain_id = f.u64()?;
                let nonce = f.u64()?;
                let gas_price = f.u128()?;
                let gas_limit = f.u64()?;
                let to = f.kind()?;
                let value = f.u256()?;
                let access_list = get_access_list(f.r)?;
                let input = Bytes::copy_from_slice(f.r.rest);
                let tx = TxEip2930 { chain_id, nonce, gas_price, gas_limit, to, value, access_list, input };
                BaseTxEnvelope::Eip2930(Signed::new_unhashed(tx, signature))
            }
            (IDENTIFIER_EIP1559, _) => {
                let mut f = F { flags: r.flags(4)?, r: &mut r };
                let chain_id = f.u64()?;
                let nonce = f.u64()?;
                let gas_limit = f.u64()?;
                let max_fee_per_gas = f.u128()?;
                let max_priority_fee_per_gas = f.u128()?;
                let to = f.kind()?;
                let value = f.u256()?;
                let access_list = get_access_list(f.r)?;
                let input = Bytes::copy_from_slice(f.r.rest);
                let tx = TxEip1559 { chain_id, nonce, gas_limit, max_fee_per_gas, max_priority_fee_per_gas, to, value, access_list, input };
                BaseTxEnvelope::Eip1559(Signed::new_unhashed(tx, signature))
            }
            (_, EIP7702_TYPE) => {
                let mut f = F { flags: r.flags(4)?, r: &mut r };
                let chain_id = f.u64()?;
                let nonce = f.u64()?;
                let gas_limit = f.u64()?;
                let max_fee_per_gas = f.u128()?;
                let max_priority_fee_per_gas = f.u128()?;
                let to = f.r.address()?;
                let value = f.u256()?;
                let access_list = get_access_list(f.r)?;
                let authorization_list = get_authorizations(f.r)?;
                let input = Bytes::copy_from_slice(f.r.rest);
                let tx = TxEip7702 {
                    chain_id,
                    nonce,
                    gas_limit,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    to,
                    value,
                    access_list,
                    authorization_list,
                    input,
                };
                BaseTxEnvelope::Eip7702(Signed::new_unhashed(tx, signature))
            }
            (_, DEPOSIT_TYPE) => {
                let flags = r.flags(2)?;
                let source_hash = r.b256()?;
                let from = r.address()?;
                let mut f = F { flags, r: &mut r };
                let to = f.kind()?;
                let mint = f.opt()?.unwrap_or(0);
                let value = f.u256()?;
                let gas_limit = f.u64()?;
                let is_system_transaction = f.bool();
                let input = Bytes::copy_from_slice(f.r.rest);
                BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
                    source_hash,
                    from,
                    to,
                    mint,
                    value,
                    gas_limit,
                    is_system_transaction,
                    input,
                }))
            }
            (_, t) if t == base_common_consensus::BASE_TX_TYPE_ID => {
                let mut f = F { flags: r.flags(6)?, r: &mut r };
                let chain_id = f.u64()?;
                let sender = f.opt_address()?;
                let nonce_key = f.u256()?;
                let nonce_sequence = f.u64()?;
                let valid_after = f.u64()?;
                let valid_before = f.u64()?;
                let max_priority_fee_per_gas = f.u128()?;
                let max_fee_per_gas = f.u128()?;
                let gas_limit = f.u64()?;
                let payer = f.opt_address()?;
                let tail_len = usize::try_from(f.u64()?).map_err(|_| CodecError("tail length"))?;
                let (account_changes, calls, metadata) = decode_base_tail(f.r.take(tail_len)?)?;
                let mut auth = f.r.rest;
                let bad = |_| CodecError("bad Base transaction auth");
                let header = alloy_rlp::Header::decode(&mut auth).map_err(bad)?;
                let before = auth.len();
                let sender_auth = Bytes::decode(&mut auth).map_err(bad)?;
                let payer_auth = Bytes::decode(&mut auth).map_err(bad)?;
                if !header.list || before - auth.len() != header.payload_length || !auth.is_empty() {
                    return Err(CodecError("bad Base transaction auth list"));
                }
                let tx = base_common_consensus::TxBase {
                    chain_id,
                    sender,
                    nonce_key,
                    nonce_sequence,
                    valid_after,
                    valid_before,
                    max_priority_fee_per_gas,
                    max_fee_per_gas,
                    gas_limit,
                    account_changes,
                    calls,
                    metadata,
                    payer,
                };
                BaseTxEnvelope::Base(base_common_consensus::TxBaseSigned::new(tx, sender_auth, payer_auth))
            }
            _ => return Err(CodecError("unsupported transaction type")),
        };
        Ok(tx)
    }
}

/// Receipt static-file rows.
#[derive(Debug)]
pub struct ReceiptCodec;

impl ReceiptCodec {
    /// Encodes `receipt` as base-reth stores it.
    pub fn encode(receipt: &BaseReceipt) -> Vec<u8> {
        let mut w = W::default();
        let (inner, ext, deposit, phases): (&Receipt<Log>, Option<u8>, Option<&DepositReceipt>, u64) = match receipt {
            BaseReceipt::Legacy(r) => (r, None, None, 0),
            BaseReceipt::Eip2930(r) => (r, None, None, 0),
            BaseReceipt::Eip1559(r) => (r, None, None, 0),
            BaseReceipt::Eip7702(r) => (r, Some(EIP7702_TYPE), None, 0),
            BaseReceipt::Deposit(d) => (&d.inner, Some(DEPOSIT_TYPE), Some(d), 0),
            BaseReceipt::Base(b) => (&b.inner, Some(base_common_consensus::BASE_TX_TYPE_ID), None, b.committed_phases),
        };
        let identifier = match receipt {
            BaseReceipt::Legacy(_) => IDENTIFIER_LEGACY,
            BaseReceipt::Eip2930(_) => IDENTIFIER_EIP2930,
            BaseReceipt::Eip1559(_) => IDENTIFIER_EIP1559,
            _ => IDENTIFIER_EXTENDED,
        };
        w.flags.put(2, identifier);
        if let Some(ext) = ext {
            w.body.push(ext);
        }
        w.bool(inner.status.coerce_status());
        w.u64(inner.cumulative_gas_used);
        put_varuint(&mut w.body, inner.logs.len());
        for log in &inner.logs {
            let mut tmp = Vec::with_capacity(64 + log.data.data.len());
            tmp.extend_from_slice(log.address.as_slice());
            put_varuint(&mut tmp, log.data.topics().len());
            for topic in log.data.topics() {
                tmp.extend_from_slice(topic.as_slice());
            }
            tmp.extend_from_slice(&log.data.data);
            put_varuint(&mut w.body, tmp.len());
            w.body.extend_from_slice(&tmp);
        }
        w.opt(deposit.and_then(|d| d.deposit_nonce).map(u128::from));
        w.opt(deposit.and_then(|d| d.deposit_receipt_version).map(u128::from));
        put_compact_u64(&mut w.body, phases);
        let zstd = w.body.len() > 7;
        w.flags.put(1, u64::from(zstd));
        let mut out = Vec::with_capacity(w.body.len() + 2);
        w.flags.write(&mut out);
        if zstd {
            out.extend_from_slice(&RECEIPT_COMPRESSOR.with_borrow_mut(|c| c.compress(&w.body)).expect("zstd compress"));
        } else {
            out.extend_from_slice(&w.body);
        }
        out
    }

    /// Decodes a stored receipt.
    pub fn decode(bytes: &[u8]) -> Decoded<BaseReceipt> {
        let mut outer = Reader::new(bytes);
        let mut flags = outer.flags(2)?;
        let body = if (flags.bits >> 9) & 1 == 1 { decompress(&RECEIPT_DECOMPRESSOR, outer.rest)? } else { outer.rest.to_vec() };
        let mut r = Reader::new(&body);
        let identifier = flags.get(2);
        let ty = if identifier == IDENTIFIER_EXTENDED { r.u8()? } else { identifier as u8 };
        let mut f = F { flags, r: &mut r };
        let success = f.bool();
        let cumulative_gas_used = f.u64()?;
        let count = f.r.varuint()?;
        let mut logs = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let len = f.r.varuint()?;
            let mut l = Reader::new(f.r.take(len)?);
            let address: Address = l.address()?;
            let topics_len = l.varuint()?;
            let mut topics: Vec<B256> = Vec::with_capacity(topics_len.min(4));
            for _ in 0..topics_len {
                topics.push(l.b256()?);
            }
            logs.push(Log { address, data: LogData::new_unchecked(topics, Bytes::copy_from_slice(l.rest)) });
        }
        let deposit_nonce = f.opt()?.map(|v| v as u64);
        let deposit_receipt_version = f.opt()?.map(|v| v as u64);
        let rest = f.r.rest.len();
        let phases = f.r.compact_u64(rest)?;
        let inner = Receipt { status: success.into(), cumulative_gas_used, logs };
        Ok(match (identifier, ty) {
            (IDENTIFIER_LEGACY, _) => BaseReceipt::Legacy(inner),
            (IDENTIFIER_EIP2930, _) => BaseReceipt::Eip2930(inner),
            (IDENTIFIER_EIP1559, _) => BaseReceipt::Eip1559(inner),
            (_, EIP7702_TYPE) => BaseReceipt::Eip7702(inner),
            (_, DEPOSIT_TYPE) => BaseReceipt::Deposit(DepositReceipt { inner, deposit_nonce, deposit_receipt_version }),
            (_, t) if t == base_common_consensus::BASE_TX_TYPE_ID => {
                BaseReceipt::Base(base_common_consensus::TxBaseReceipt::new(inner, phases))
            }
            _ => return Err(CodecError("unsupported receipt type")),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip2718::Encodable2718;

    use super::*;
    use crate::{Rocks, RocksTable, Segment, StaticFiles};

    #[test]
    fn base_native_transactions_round_trip() {
        let tx = base_common_consensus::TxBase {
            chain_id: 8453,
            sender: Some(Address::repeat_byte(1)),
            nonce_key: U256::from(7),
            nonce_sequence: 3,
            valid_after: 10,
            valid_before: 1_000,
            max_priority_fee_per_gas: 5,
            max_fee_per_gas: 1_000_000,
            gas_limit: 90_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::from_static(b"meta"),
            payer: None,
        };
        let signed = base_common_consensus::TxBaseSigned::new(tx, Bytes::from(vec![9u8; 85]), Bytes::new());
        let envelope = BaseTxEnvelope::Base(signed);
        let bytes = TxCodec::encode(&envelope).unwrap();
        assert_eq!(bytes[0], 3 << 1, "extended identifier, no zstd, zero parity");
        assert_eq!(TxCodec::decode(&bytes).unwrap(), envelope);
        let receipt = BaseReceipt::Base(base_common_consensus::TxBaseReceipt::new(
            Receipt { status: true.into(), cumulative_gas_used: 21_000, logs: Vec::new() },
            2,
        ));
        assert_eq!(ReceiptCodec::decode(&ReceiptCodec::encode(&receipt)).unwrap(), receipt);
    }

    /// Every transaction and receipt of a base-reth datadir (set `TIGER_RETH_DATADIR`) decodes,
    /// re-encodes to the same bytes, and hashes to its `TransactionHashNumbers` entry.
    #[test]
    fn reth_datadir_rows_round_trip() {
        let Ok(datadir) = std::env::var("TIGER_RETH_DATADIR") else { return };
        let dir = std::path::Path::new(&datadir);
        let sf = StaticFiles::open(&dir.join("static_files")).unwrap();
        let rocks = Rocks::open(&dir.join("rocksdb")).unwrap();
        let count = sf.next_tx(Segment::Transactions);
        assert!(count > 0);
        let mut compressed = 0;
        for tx in 0..count {
            let row = sf.tx_row(Segment::Transactions, tx).unwrap().unwrap();
            compressed += usize::from(row[0] & 8 != 0);
            let decoded = TxCodec::decode(&row).unwrap();
            assert_eq!(TxCodec::encode(&decoded).unwrap(), row, "tx {tx}");
            let hash = decoded.trie_hash();
            let number = rocks.get(RocksTable::TransactionHashNumbers, hash.as_slice()).unwrap().expect("hash indexed");
            assert_eq!(u64::from_le_bytes(number[..8].try_into().unwrap()), tx);
            let row = sf.tx_row(Segment::Receipts, tx).unwrap().unwrap();
            let receipt = ReceiptCodec::decode(&row).unwrap();
            assert_eq!(ReceiptCodec::encode(&receipt), row, "receipt {tx}");
        }
        assert!(compressed > 0, "some rows are zstd-compressed");
    }
}
