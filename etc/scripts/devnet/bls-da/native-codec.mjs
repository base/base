// Development-only native type 0x7a. This is a wire client, not EVM calldata.
import assert from 'node:assert/strict';
import { bls12_381 as curve } from '@noble/curves/bls12-381.js';
import { concat, decodeRlp, encodeRlp, getBytes, hexlify, keccak256, recoverAddress,
  solidityPacked, toBeHex, toUtf8Bytes } from 'ethers';

export const CHAIN_ID = 84538453n;
export const TYPE = '0x7a';
export const REGISTRY = '0x43000000000000000000000000000000000000b1';
export const DST = 'BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_';
export const POP = 'BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_';
export const DOMAIN = keccak256(toUtf8Bytes('base-native-bls:v1:eth-transfer'));
export const bls = curve.shortSignatures;
export const rlpInt = n => BigInt(n) === 0n ? '0x' : toBeHex(BigInt(n));
export const signatureBytes = point => hexlify(bls.Signature.toBytes(point));

// A wallet submits the pooled form, but blocks and DA carry only the canonical intent.
export const TRANSFER_TYPE = '0x7b';
export function transferFields(tx) {
  return [rlpInt(tx.chainId ?? CHAIN_ID), tx.sender, rlpInt(tx.nonce), tx.recipient,
    rlpInt(tx.value), rlpInt(tx.maxPriorityFeePerGas), rlpInt(tx.maxFeePerGas)];
}

export function transferIntent(tx) {
  const fields = transferFields(tx);
  const canonical = concat([TRANSFER_TYPE, encodeRlp(fields)]);
  const hash = keccak256(canonical);
  return { fields, canonical, hash, digest: getBytes(keccak256(concat([DOMAIN, hash]))) };
}

export function signTransfer(tx, secretKey) {
  const intent = transferIntent(tx);
  const signature = signatureBytes(bls.sign(bls.hash(intent.digest, DST), secretKey));
  return { ...intent, signature, raw: concat([TRANSFER_TYPE, encodeRlp([intent.fields, signature])]) };
}

export function transferFromRpc(tx) {
  assert.equal(BigInt(tx.type), 123n);
  assert.equal(BigInt(tx.gas), 21_000n);
  const intent = transferIntent({ ...tx, sender: tx.sender ?? tx.from, recipient: tx.recipient ?? tx.to });
  assert.equal(intent.hash, tx.hash, 'Intent RPC fields must reproduce the canonical hash');
  return intent;
}

export function registration(key) {
  const publicKey = key.publicKey.toBytes(true);
  const proof = signatureBytes(bls.sign(bls.hash(publicKey, POP), key.secretKey));
  return { publicKey: hexlify(publicKey), proof, input: concat(['0x00', publicKey, proof]) };
}

export function record(user, overrides = {}) {
  return solidityPacked(['address', 'uint64', 'address', 'uint96'], [
    user.owner.address, overrides.nonce ?? user.nonce,
    overrides.recipient ?? user.recipient, overrides.value ?? user.value,
  ]);
}

export function digest(recordBytes, chainId = CHAIN_ID) {
  return getBytes(keccak256(concat([DOMAIN, toBeHex(chainId, 32), recordBytes])));
}

export function batch(users, overrides = {}) {
  const records = users.map(u => record(u, overrides));
  const signature = signatureBytes(bls.aggregateSignatures(users.map((u, i) =>
    bls.sign(bls.hash(digest(records[i], overrides.chainId ?? CHAIN_ID), DST), u.secretKey))));
  return { records, signature, input: concat(['0x01', toBeHex(users.length, 1), ...records, signature]) };
}

export function signNative(wallet, input, { nonce, chainId = CHAIN_ID,
  gasLimit = 5_500_000n, maxPriorityFeePerGas = 1_000_000n, maxFeePerGas = 100_000_000_000n }) {
  const fields = [chainId, nonce, maxPriorityFeePerGas, maxFeePerGas, gasLimit].map(rlpInt);
  fields.push(input);
  const signature = wallet.signingKey.sign(keccak256(concat([TYPE, encodeRlp(fields)])));
  return concat([TYPE, encodeRlp([...fields, rlpInt(signature.yParity), rlpInt(signature.r), rlpInt(signature.s)])]);
}

export function decodeNative(raw) {
  assert.equal(raw.slice(0, 4), TYPE);
  const fields = decodeRlp(`0x${raw.slice(4)}`);
  assert(Array.isArray(fields) && fields.length === 9 && fields.every(f => typeof f === 'string'));
  assert.equal(concat([TYPE, encodeRlp(fields)]), raw, 'Noncanonical native RLP');
  const integer = (index, maxBytes) => {
    const bytes = getBytes(fields[index]);
    assert(bytes.length <= maxBytes && (bytes.length === 0 || bytes[0] !== 0), 'Noncanonical integer');
    return fields[index] === '0x' ? 0n : BigInt(fields[index]);
  };
  const chainId = integer(0, 8), nonce = integer(1, 8);
  const maxPriorityFeePerGas = integer(2, 16), maxFeePerGas = integer(3, 16), gasLimit = integer(4, 8);
  const yParity = integer(6, 1);
  assert(yParity <= 1n);
  const signature = { r: toBeHex(integer(7, 32), 32), s: toBeHex(integer(8, 32), 32), yParity: Number(yParity) };
  const signingHash = keccak256(concat([TYPE, encodeRlp(fields.slice(0, 6))]));
  return { hash: keccak256(raw), sender: recoverAddress(signingHash, signature), chainId, nonce,
    gasLimit, maxPriorityFeePerGas, maxFeePerGas, input: fields[5], signature };
}

// Reconstruct from JSON-RPC rather than using ethers' closed transaction-type enum.
export function rawFromRpc(tx) {
  assert.equal(BigInt(tx.type), 122n);
  const fields = [tx.chainId, tx.nonce, tx.maxPriorityFeePerGas, tx.maxFeePerGas, tx.gas].map(rlpInt);
  fields.push(tx.input, rlpInt(tx.yParity ?? tx.v), rlpInt(tx.r), rlpInt(tx.s));
  const raw = concat([TYPE, encodeRlp(fields)]);
  assert.equal(keccak256(raw), tx.hash, 'Native RPC fields must reproduce the transaction hash');
  assert.equal(decodeNative(raw).sender.toLowerCase(), tx.from.toLowerCase());
  return raw;
}

export function decodePayload(input) {
  const bytes = getBytes(input);
  if (bytes[0] === 0) {
    assert.equal(bytes.length, 145);
    return { kind: 'registration', publicKey: hexlify(bytes.slice(1, 97)), proof: hexlify(bytes.slice(97)) };
  }
  assert.equal(bytes[0], 1);
  const count = bytes[1];
  assert(count >= 1 && count <= 32);
  assert.equal(bytes.length, 2 + count * 60 + 48);
  const records = Array.from({ length: count }, (_, i) => hexlify(bytes.slice(2 + i * 60, 62 + i * 60)));
  const senders = records.map(r => `0x${r.slice(2, 42)}`);
  assert(senders.every((sender, i) => i === 0 || sender > senders[i - 1]), 'Senders must be strictly ordered');
  return { kind: 'batch', records, senders, signature: hexlify(bytes.slice(-48)) };
}
