// Counterfactual native-format sizing, NOT executable Base transaction encoding.
// Preserve real transaction payloads, replace only outer ECDSA authorization.
// All registry entries are charged in the cold case; warm assumes those same
// entries were previously published on L1. No off-chain key lookup is assumed.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import { brotliCompressSync, constants } from 'node:zlib';
import { Transaction, concat, encodeRlp, getBytes, keccak256, toBeHex } from 'ethers';

const url = process.env.SAMPLE_RPC_URL ?? 'https://mainnet.base.org';
let requestId = 0;
async function rpc(method, params) {
  const response = await fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++requestId, method, params }), signal: AbortSignal.timeout(30_000) });
  assert(response.ok, `RPC HTTP ${response.status}`);
  const json = await response.json();
  assert(!json.error, JSON.stringify(json.error));
  return json.result;
}
const head = process.env.SAMPLE_END_BLOCK ? BigInt(process.env.SAMPLE_END_BLOCK) : BigInt(await rpc('eth_blockNumber', []));
const blocks = [];
for (let i = 7n; i >= 0n; i--) {
  blocks.push(await rpc('eth_getBlockByNumber', [toBeHex(head - i), true]));
}

// Random-looking deterministic placeholders model compressed point entropy only.
// These are not keys/signatures and cannot authorize the sampled transactions.
const entropy = (label, size) => concat(Array.from({ length: Math.ceil(size / 32) }, (_, i) =>
  `0x${createHash('sha256').update(`${label}:${i}`).digest('hex')}`)).slice(0, 2 + size * 2);
const rlpInt = value => BigInt(value) === 0n ? '0x' : toBeHex(BigInt(value));
const sig = item => ({ r: item.r, s: item.s, yParity: Number(item.yParity ?? (BigInt(item.v) & 1n)) });
function transaction(tx) {
  const type = Number(tx.type);
  const t = Transaction.from({ type, chainId: BigInt(tx.chainId ?? '8453'), nonce: Number(tx.nonce),
    gasLimit: BigInt(tx.gas), to: tx.to, value: BigInt(tx.value), data: tx.input,
    gasPrice: type < 2 ? BigInt(tx.gasPrice) : undefined,
    maxFeePerGas: type >= 2 ? BigInt(tx.maxFeePerGas) : undefined,
    maxPriorityFeePerGas: type >= 2 ? BigInt(tx.maxPriorityFeePerGas) : undefined,
    accessList: tx.accessList,
    authorizationList: tx.authorizationList?.map(a => ({ address: a.address,
      chainId: BigInt(a.chainId), nonce: BigInt(a.nonce), signature: sig(a) })),
    signature: type === 0 ? { r: tx.r, s: tx.s, v: BigInt(tx.v) } : sig(tx),
  });
  assert.equal(t.hash, tx.hash, `RPC reconstruction mismatch: ${tx.hash}`);
  assert.equal(t.from.toLowerCase(), tx.from.toLowerCase());
  return t;
}

const originals = [], unsigned = [], addresses = new Set(), typeCounts = {};
let skipped = 0, total = 0;
for (const block of blocks) {
  const original = [], changed = [];
  for (const tx of block.transactions) {
    if (Number(tx.type) === 126) continue; // Deposits are derived from L1, not batch data.
    assert([0, 1, 2, 4].includes(Number(tx.type)), `Unsupported non-deposit type ${tx.type}; refusing a partial estimate`);
    const t = transaction(tx);
    total++;
    typeCounts[t.type] = (typeCounts[t.type] ?? 0) + 1;
    original.push(t.serialized);
    // EIP-7702's nested authorization signatures remain intact.
    changed.push([tx.from, t.unsignedSerialized]);
    addresses.add(tx.from.toLowerCase());
  }
  const metadata = [block.parentHash, rlpInt(100), keccak256('0x1234'), rlpInt(block.timestamp)];
  originals.push(encodeRlp(concat(['0x00', encodeRlp([...metadata, original])])));
  unsigned.push({ metadata, changed, hash: block.hash });
}
function size(entries) {
  const bytes = getBytes(concat(entries));
  return 1 + brotliCompressSync(bytes, { params: {
    [constants.BROTLI_PARAM_QUALITY]: 9, [constants.BROTLI_PARAM_LGWIN]: 22,
  } }).length;
}
const baseline = size(originals);
const budget = 14 * (130_044 - 24) / 12;
const variants = [];
for (const [signatureBytes, publicKeyBytes] of [[48, 96], [96, 48]]) {
  // Full 20-byte sender addresses, not free/optimistically short registry IDs.
  // A future native transaction format must bind sender, nonce, chain and fields.
  const replacement = unsigned.map(b => encodeRlp([
    '0xb1', ...b.metadata, b.changed, entropy(`aggregate:${b.hash}`, signatureBytes),
  ]));
  const keys = [...addresses].map(a => [a, entropy(`public-key:${a}`, publicKeyBytes)]);
  const warm = size(replacement);
  const cold = size([encodeRlp(['0xb2', keys]), ...replacement]);
  const migratedKeys = keys.map(([a, pk]) => [a, pk, entropy(`ECDSA migration:${a}`, 65)]);
  const migrated = size([encodeRlp(['0xb2', migratedKeys]), ...replacement]);
  const migratedWithPop = size([encodeRlp(['0xb2', migratedKeys.map(([a, pk, auth]) =>
    [a, pk, auth, entropy(`PoP:${a}`, signatureBytes)])]), ...replacement]);
  variants.push({ signatureBytes, publicKeyBytes, aggregates: blocks.length,
    warmBytes: warm, coldBytes: cold,
    coldWithEcdsaMigrationBytes: migrated, coldWithEcdsaMigrationAndPopBytes: migratedWithPop,
    warmSavingsPercent: 100 * (baseline - warm) / baseline,
    coldSavingsPercent: 100 * (baseline - cold) / baseline,
    coldWithEcdsaMigrationSavingsPercent: 100 * (baseline - migrated) / baseline,
    coldWithEcdsaMigrationAndPopSavingsPercent: 100 * (baseline - migratedWithPop) / baseline,
    warmThroughputMultiplier: baseline / warm, coldThroughputMultiplier: baseline / cold,
    warmBytesPerTx: warm / total, coldBytesPerTx: cold / total,
    allL1TargetWarmTps: budget * total / warm, allL1TargetColdTps: budget * total / cold,
  });
}
const report = { source: url, blockNumbers: blocks.map(b => Number(b.number)), blockHashes: blocks.map(b => b.hash),
  sampleFirstTimestamp: new Date(Number(blocks[0].timestamp) * 1000).toISOString(),
  sampleLastTimestamp: new Date(Number(blocks.at(-1).timestamp) * 1000).toISOString(),
  transactions: total, uniqueSenders: addresses.size, typeCounts, skipped,
  baselineCompressedBytes: baseline, baselineBytesPerTx: baseline / total,
  allL1TargetBaselineTps: budget * total / baseline, variants,
  caveats: [
    'Counterfactual native protocol sizing, NOT implemented transaction encoding or valid BLS signatures.',
    'Full sender addresses retained; account registry state must be reconstructable from L1.',
    'Cold case includes every public key and address once, but NOT migration authorization/registration envelopes or PoPs: optimistic lower bound on cold bytes.',
    'Migration columns add 65-byte ECDSA authorization and optionally a PoP per unique sender; separate transaction envelopes/gas fields are still excluded (batched native registration assumption).',
    'Warm case assumes prior L1-published registration; charge that historical cost when amortizing.',
    'Only outer signatures replaced; calldata/access lists/EIP-7702 authorizations preserved.',
    'One aggregate per block; synthetic L1 origin metadata; quality-9 Brotli, no claim of exact live batcher channel sizes.',
    'TPS assumes full utilization of the entire 14-blob/12-second L1 target; multiply by Base share and fill ratio; ignores execution/proving limits.',
    'Small contiguous sample, not a workload forecast or measurement of current L1 saturation.',
  ] };
const json = JSON.stringify(report, null, 2);
if (process.argv[2]) writeFileSync(process.argv[2], `${json}\n`);
console.log(json);
