// Checker inputs are never copied into the replay node. Recover keys, PoPs,
// intents and chunk signatures from L1 blob sidecars, not the producer's witness RPC.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';
import { brotliDecompressSync } from 'node:zlib';
import { bls12_381 as curve } from '@noble/curves/bls12-381.js';
import { JsonRpcProvider, concat, decodeRlp, getBytes, hexlify, keccak256, toQuantity } from 'ethers';
import { CHAIN_ID, DOMAIN, DST, POP, bls, decodeNative, decodePayload, rawFromRpc,
  transferFromRpc } from './native-codec.mjs';

const expected = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const provider = new JsonRpcProvider(process.env.VALIDATOR_RPC_URL ?? 'http://127.0.0.1:8645', undefined, { cacheTimeout: -1 });
const l1 = new JsonRpcProvider(process.env.L1_RPC_URL ?? 'http://127.0.0.1:4545', undefined, { cacheTimeout: -1 });
const beacon = process.env.L1_BEACON_URL ?? 'http://127.0.0.1:4052';
assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
assert.equal((await l1.getNetwork()).chainId, 1337n);
assert.equal(await provider.send('net_peerCount', []), '0x0');
const active = await fetch(process.env.VALIDATOR_CL_RPC_URL ?? 'http://127.0.0.1:8649', {
  method: 'POST', headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'admin_sequencerActive', params: [] }),
  signal: AbortSignal.timeout(30_000),
});
assert(active.ok); assert.equal((await active.json()).result, false);
let safe;
const deadline = Date.now() + 600_000;
while (Date.now() < deadline) {
  safe = await provider.send('eth_getBlockByNumber', ['safe', false]);
  if (safe && BigInt(safe.number) >= BigInt(expected.checkpoint.number)) break;
  console.error(`L1-only safe head ${safe?.number ?? 'none'} -> ${expected.checkpoint.number}`);
  await sleep(5_000);
}
assert(safe && BigInt(safe.number) >= BigInt(expected.checkpoint.number));
const checkpoint = await provider.send('eth_getBlockByNumber', [expected.checkpoint.number, false]);
for (const field of ['hash', 'stateRoot', 'transactionsRoot', 'receiptsRoot']) {
  assert.equal(checkpoint[field], expected.checkpoint[field]);
}
for (const account of expected.expectedAccounts) {
  assert.equal((await provider.getBalance(account.address)).toString(), account.balance);
  assert.equal(await provider.getTransactionCount(account.address), account.nonce);
  assert.equal((await provider.getBalance(account.recipient)).toString(), account.recipientBalance);
}
for (const receipt of expected.receipts) {
  const hash = receipt.transactionHash ?? receipt.hash;
  const derived = await provider.send('eth_getTransactionReceipt', [hash]);
  assert(derived);
  for (const field of ['blockNumber', 'blockHash', 'status', 'type', 'gasUsed']) {
    assert.equal(derived[field], receipt[field]);
  }
}
for (const block of expected.blocks) {
  const derived = await provider.send('eth_getBlockByNumber', [block.number, true]);
  assert.equal(derived.hash, block.hash);
  assert.deepEqual(derived.transactions.filter(tx => BigInt(tx.type) === 123n)
    .map(tx => transferFromRpc(tx).canonical), block.intents);
}
for (const reg of expected.registrations) {
  assert.equal(rawFromRpc(await provider.send('eth_getTransactionByHash', [reg.hash])), reg.raw);
}

// Independent JS implementation of Base's 4-field-element / 127-byte blob packing.
function decodeBlob(hex) {
  const blob = Buffer.from(getBytes(hex));
  assert.equal(blob.length, 131_072); assert.equal(blob[1], 0); assert(blob[0] < 64);
  const length = blob.readUIntBE(2, 3);
  assert(length <= 130_044);
  const output = Buffer.alloc(130_044);
  blob.copy(output, 0, 5, 32);
  let input = 32, position = 28;
  function field() {
    const high = blob[input]; assert(high < 64);
    blob.copy(output, position, input + 1, input + 32);
    input += 32; position += 32;
    return high;
  }
  function unpack(high) {
    position--;
    output[position - 96] = high[0] | ((high[1] & 48) << 2);
    output[position - 64] = (high[1] & 15) | ((high[3] & 15) << 4);
    output[position - 32] = high[2] | ((high[3] & 48) << 2);
  }
  unpack([blob[0], field(), field(), field()]);
  while (position < length) unpack([field(), field(), field(), field()]);
  assert(output.subarray(length).every(byte => byte === 0));
  assert(blob.subarray(input).every(byte => byte === 0));
  return output.subarray(0, length);
}

function channelItems(compressed) {
  assert.equal(compressed[0], 1, 'expected a Brotli channel');
  const bytes = brotliDecompressSync(compressed.subarray(1), { maxOutputLength: 10_000_000 });
  const items = [];
  for (let offset = 0; offset < bytes.length;) {
    const prefix = bytes[offset];
    assert(prefix >= 0x80 && prefix < 0xc0, 'channel items are RLP strings');
    const header = prefix <= 0xb7 ? 1 : 1 + prefix - 0xb7;
    const length = header === 1 ? prefix - 0x80 : bytes.readUIntBE(offset + 1, header - 1);
    assert(offset + header + length <= bytes.length);
    items.push(getBytes(decodeRlp(hexlify(bytes.subarray(offset, offset + header + length)))));
    offset += header + length;
  }
  return items;
}

const keys = new Map(), published = new Set(), channels = new Map();
const chunks = [], blobTransactions = [];
function verifyItem(bytes) {
  const type = bytes[0], payload = decodeRlp(hexlify(bytes.subarray(1)));
  assert(type === 0 || type === 2, `unexpected devnet batch type ${type}`);
  const [aggregate, batches] = type === 2 ? payload : ['0x', [payload]];
  assert(batches.length > 0 && batches.length <= 128);
  const signers = [];
  for (const batch of batches) {
    assert.equal(batch.length, 5);
    for (const raw of batch[4]) {
      if (raw.startsWith('0x7a')) {
        const registration = decodeNative(raw);
        const data = decodePayload(registration.input);
        assert.equal(registration.chainId, CHAIN_ID);
        assert.equal(data.kind, 'registration');
        const key = curve.G2.Point.fromBytes(getBytes(data.publicKey));
        assert(bls.verify(bls.Signature.fromBytes(getBytes(data.proof)), bls.hash(getBytes(data.publicKey), POP), key));
        const owner = registration.sender.toLowerCase();
        if (keys.has(owner)) assert.deepEqual(keys.get(owner).toBytes(true), key.toBytes(true));
        keys.set(owner, key);
      } else if (raw.startsWith('0x7b')) {
        assert.equal(type, 2, 'intent outside an authenticated chunk');
        const fields = decodeRlp(`0x${raw.slice(4)}`);
        assert.equal(fields.length, 7); assert.equal(BigInt(fields[0]), CHAIN_ID);
        const key = keys.get(fields[1].toLowerCase());
        assert(key, 'key and PoP must be published before the transfer');
        const hash = keccak256(raw);
        signers.push({ message: bls.hash(getBytes(keccak256(concat([DOMAIN, hash]))), DST), publicKey: key });
        published.add(hash);
      }
    }
  }
  assert(signers.length <= 16_384);
  assert.equal(aggregate !== '0x', signers.length > 0);
  if (signers.length) {
    assert.equal(getBytes(aggregate).length, 48);
    assert(bls.verifyBatch(bls.Signature.fromBytes(getBytes(aggregate)), signers), 'L1 aggregate does not authorize its intents');
  }
  chunks.push({ blocks: batches.length, transfers: signers.length, aggregateBytes: signers.length ? 48 : 0 });
}

async function beaconGet(path) {
  const response = await fetch(`${beacon}${path}`, { signal: AbortSignal.timeout(30_000) });
  assert(response.ok, `beacon request failed: ${path} (${response.status})`);
  return (await response.json()).data;
}
const genesisTime = BigInt((await beaconGet('/eth/v1/beacon/genesis')).genesis_time);
const slotSeconds = BigInt((await beaconGet('/eth/v1/config/spec')).SECONDS_PER_SLOT);
const l1Head = await l1.getBlockNumber();
for (let height = 0; height <= l1Head; height++) {
  const block = await l1.send('eth_getBlockByNumber', [toQuantity(height), true]);
  const txs = block.transactions.filter(tx => BigInt(tx.type) === 3n);
  if (!txs.length) continue;
  const slot = (BigInt(block.timestamp) - genesisTime) / slotSeconds;
  const sidecars = await beaconGet(`/eth/v1/beacon/blob_sidecars/${slot}`);
  const byHash = new Map(sidecars.map(sidecar => {
    const hash = createHash('sha256').update(getBytes(sidecar.kzg_commitment)).digest(); hash[0] = 1;
    return [hexlify(hash), sidecar];
  }));
  for (const tx of txs) {
    assert.equal((await l1.getTransactionReceipt(tx.hash)).status, 1);
    blobTransactions.push({ hash: tx.hash, blockNumber: height, slot: slot.toString(), blobs: tx.blobVersionedHashes.length });
    for (const hash of tx.blobVersionedHashes) {
      const sidecar = byHash.get(hash); assert(sidecar, 'L1 blob hash must match a beacon commitment');
      const data = decodeBlob(sidecar.blob); assert.equal(data[0], 0);
      for (let offset = 1; offset < data.length;) {
        assert(offset + 23 <= data.length);
        const id = hexlify(data.subarray(offset, offset + 16));
        const number = data.readUInt16BE(offset + 16), length = data.readUInt32BE(offset + 18);
        assert(offset + 23 + length <= data.length);
        const fragment = data.subarray(offset + 22, offset + 22 + length);
        const last = data[offset + 22 + length]; assert(last === 0 || last === 1);
        const channel = channels.get(id) ?? { frames: new Map(), last: undefined, verified: false };
        if (channel.frames.has(number)) assert.deepEqual(channel.frames.get(number), fragment);
        channel.frames.set(number, fragment);
        if (last) {
          if (channel.last !== undefined) assert.equal(channel.last, number);
          channel.last = number;
        }
        channels.set(id, channel);
        if (!channel.verified && channel.last !== undefined
          && Array.from({ length: channel.last + 1 }, (_, i) => channel.frames.has(i)).every(Boolean)) {
          const compressed = Buffer.concat(Array.from({ length: channel.last + 1 }, (_, i) => channel.frames.get(i)));
          for (const item of channelItems(compressed)) verifyItem(item);
          channel.verified = true;
        }
        offset += 23 + length;
      }
    }
  }
}
assert(blobTransactions.length > 0);
for (const block of expected.blocks) for (const intent of block.intents) assert(published.has(keccak256(intent)));
assert.equal(expected.blocks.reduce((n, block) => n + block.intents.length, 0), expected.transfers);
const report = { result: 'PASS', checkpoint: expected.checkpoint, safeHead: { number: safe.number, hash: safe.hash },
  binarySha256: createHash('sha256').update(readFileSync(process.env.NATIVE_BASE_BINARY ??
    new URL('../../../../target/debug/base', import.meta.url))).digest('hex'),
  executionPeers: 0, sequencerActive: false, checkedReceipts: expected.receipts.length,
  checkedAccounts: expected.expectedAccounts.length, l1PublishedKeys: keys.size,
  l1PublishedUniqueIntents: published.size, expectedTransfers: expected.transfers, chunks, blobTransactions,
  authorizationSource: 'Keys, PoPs, transfer intents and chunk aggregates decoded from L1 beacon blob sidecars; no producer witness RPC or saved aggregate used.',
  scope: 'Fresh L1-isolated node matches roots/receipts/balances; independent JS blob/frame/chunk decoding and noble PoP/aggregate verification. Commitment hashes matched to canonical L1 transactions. KZG verification is performed by the node/beacon stack, not independently by this JS checker. Not an audit, formal security proof or proving benchmark.' };
const json = JSON.stringify(report, null, 2);
if (process.argv[3]) writeFileSync(process.argv[3], `${json}\n`);
console.log(json);
provider.destroy(); l1.destroy();
