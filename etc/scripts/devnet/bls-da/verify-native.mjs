// Read-only verification of a FRESH L1-isolated native Base node. Expected data
// is used only by this checker; never imported into the node or used to build it.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';
import { bls12_381 as curve } from '@noble/curves/bls12-381.js';
import { JsonRpcProvider, getBytes, toBeHex } from 'ethers';
import { CHAIN_ID, DST, POP, bls, decodeNative, decodePayload, digest, rawFromRpc } from './native-codec.mjs';

const expected = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const provider = new JsonRpcProvider(process.env.VALIDATOR_RPC_URL ?? 'http://127.0.0.1:8645',
  undefined, { cacheTimeout: -1 });
const l1 = new JsonRpcProvider(process.env.L1_RPC_URL ?? 'http://127.0.0.1:4545',
  undefined, { cacheTimeout: -1 });
assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
assert.equal((await l1.getNetwork()).chainId, 1337n);
assert.equal(await provider.send('net_peerCount', []), '0x0');
const response = await fetch(process.env.VALIDATOR_CL_RPC_URL ?? 'http://127.0.0.1:8649', {
  method: 'POST', headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'admin_sequencerActive', params: [] }),
  signal: AbortSignal.timeout(30_000),
});
assert(response.ok);
assert.equal((await response.json()).result, false);
const deadline = Date.now() + 600_000;
let safe;
while (Date.now() < deadline) {
  safe = await provider.send('eth_getBlockByNumber', ['safe', false]);
  if (safe && BigInt(safe.number) >= BigInt(expected.checkpoint.number)) break;
  console.error(`Waiting for L1-derived native safe head: ${safe?.number ?? 'none'} -> ${expected.checkpoint.number}`);
  await sleep(5_000);
}
assert(safe && BigInt(safe.number) >= BigInt(expected.checkpoint.number), 'Native L1 derivation timed out');
const checkpoint = await provider.send('eth_getBlockByNumber', [expected.checkpoint.number, false]);
for (const field of ['hash', 'stateRoot', 'transactionsRoot', 'receiptsRoot']) {
  assert.equal(checkpoint[field], expected.checkpoint[field], `Derived ${field} differs`);
}
for (const account of expected.expectedAccounts) {
  assert.equal((await provider.getBalance(account.address)).toString(), account.balance);
  assert.equal(await provider.getTransactionCount(account.address), account.nonce);
  assert.equal(await provider.getCode(account.address), '0x');
  assert.equal((await provider.getBalance(account.recipient)).toString(), account.recipientBalance);
}

// Independently recover the authorization material solely from the derived
// native transactions, then verify PoPs and aggregates with a different library.
const keys = new Map();
for (const registration of expected.registrations) {
  const tx = await provider.send('eth_getTransactionByHash', [registration.hash]);
  const decoded = decodeNative(rawFromRpc(tx));
  const payload = decodePayload(decoded.input);
  assert.equal(payload.kind, 'registration');
  assert.equal(decoded.sender, registration.owner);
  assert.equal(payload.publicKey, registration.publicKey);
  assert.equal(payload.proof, registration.proof);
  const key = curve.G2.Point.fromBytes(getBytes(payload.publicKey));
  assert(bls.verify(bls.Signature.fromBytes(getBytes(payload.proof)), bls.hash(getBytes(payload.publicKey), POP), key));
  keys.set(decoded.sender.toLowerCase(), key);
}
let transfers = 0;
for (const batch of expected.batches) {
  const tx = await provider.send('eth_getTransactionByHash', [batch.hash]);
  const payload = decodePayload(decodeNative(rawFromRpc(tx)).input);
  assert.equal(payload.kind, 'batch');
  assert.equal(payload.records.length, batch.transfers);
  assert.equal(getBytes(payload.signature).length, 48);
  assert(bls.verifyBatch(bls.Signature.fromBytes(getBytes(payload.signature)), payload.records.map((record, i) => {
    const key = keys.get(payload.senders[i]);
    assert(key, 'Every sender key must come from a derived PoP registration');
    return { message: bls.hash(digest(record), DST), publicKey: key };
  })));
  transfers += payload.records.length;
}
assert.equal(transfers, 1_000);
for (const receipt of expected.receipts) {
  const derived = await provider.send('eth_getTransactionReceipt', [receipt.hash]);
  assert(derived);
  for (const field of ['blockNumber', 'blockHash', 'status', 'type', 'gasUsed']) {
    assert.equal(derived[field], receipt[field], `Receipt mismatch for ${receipt.hash}: ${field}`);
  }
  if (receipt.raw) {
    assert.equal(rawFromRpc(await provider.send('eth_getTransactionByHash', [receipt.hash])), receipt.raw);
  }
}
for (const denied of expected.rejected) {
  assert.equal(await provider.send('eth_getTransactionReceipt', [denied.hash]), null);
}

const blobTransactions = [];
const l1Head = await l1.getBlockNumber();
for (let height = 0; height <= l1Head; height++) {
  const block = await l1.send('eth_getBlockByNumber', [toBeHex(height), true]);
  for (const tx of block.transactions) {
    if (BigInt(tx.type) === 3n) {
      assert.equal((await l1.getTransactionReceipt(tx.hash)).status, 1);
      blobTransactions.push({ hash: tx.hash, blockNumber: height, to: tx.to, blobVersionedHashes: tx.blobVersionedHashes });
    }
  }
}
assert(blobTransactions.length > 0);
const report = { result: 'PASS', client: await provider.send('web3_clientVersion', []),
  checkpoint: expected.checkpoint, safeHead: { number: safe.number, hash: safe.hash },
  executionPeers: 0, sequencerActive: false, nativeType: expected.type,
  checkedAccounts: expected.expectedAccounts.length, checkedPoPs: keys.size,
  checkedAggregates: expected.batches.length, derivedTransfers: transfers,
  checkedReceipts: expected.receipts.length, rejectedTransactions: expected.rejected.length,
  l1Head, blobTransactions,
  networkPrecondition: 'Fresh datadir on native-compose.yaml plus compose.yaml L1-only internal network, no copied L2 state or producer connectivity.',
  scope: 'Devnet native transaction execution and blob derivation; independent noble PoP/aggregate checks. Not production finality, a security audit, or a proof-system benchmark.' };
const json = JSON.stringify(report, null, 2);
if (process.argv[3]) writeFileSync(process.argv[3], `${json}\n`);
console.log(json);
provider.destroy();
l1.destroy();
