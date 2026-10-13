// Run against a FRESH validator started using compose.yaml's L1-only network.
// The evidence file supplies expected outputs only: never sent to the validator.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';
import { Contract, JsonRpcProvider, getBytes, keccak256, toBeHex } from 'ethers';

const expected = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const rpc = process.env.VALIDATOR_RPC_URL ?? 'http://127.0.0.1:8645';
const clRpc = process.env.VALIDATOR_CL_RPC_URL ?? 'http://127.0.0.1:8649';
const l1Rpc = process.env.L1_RPC_URL ?? 'http://127.0.0.1:4545';
// Read-only. An internal Docker network may require its bridge IP rather than a
// published localhost port. Never submit transactions or state to this node.
const validator = new JsonRpcProvider(rpc, undefined, { cacheTimeout: -1 });
const l1 = new JsonRpcProvider(l1Rpc, undefined, { cacheTimeout: -1 });
assert.equal((await validator.getNetwork()).chainId.toString(), expected.chainId);
assert.equal((await l1.getNetwork()).chainId, 1337n);
const activeResponse = await fetch(clRpc, { method: 'POST', headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'admin_sequencerActive', params: [] }),
  signal: AbortSignal.timeout(30_000) });
assert(activeResponse.ok);
const active = await activeResponse.json();
assert.equal(active.result, false, 'Replay node must not produce blocks');
const target = Number(expected.checkpoint.number);
const deadline = Date.now() + 600_000;
let safe;
while (Date.now() < deadline) {
  safe = await validator.send('eth_getBlockByNumber', ['safe', false]);
  if (safe && Number(safe.number) >= target) break;
  console.error(`Waiting for L1-derived safe head: ${safe?.number ?? 'none'} -> ${target}`);
  await sleep(5_000);
}
assert(safe && Number(safe.number) >= target, 'L1-only derivation timed out');
const block = await validator.send('eth_getBlockByNumber', [expected.checkpoint.number, false]);
for (const field of ['hash', 'stateRoot', 'receiptsRoot', 'transactionsRoot']) {
  assert.equal(block[field], expected.checkpoint[field], `Derived ${field} differs`);
}
assert.equal(keccak256(await validator.getCode(expected.contract)), expected.runtimeHash);
assert.equal(await validator.send('net_peerCount', []), '0x0');
const contract = new Contract(expected.contract,
  ['function accounts(uint32) view returns (bytes publicKey,address ecdsaOwner,uint64 nonce,uint256 balance)',
    'function count() view returns (uint32)',
    'function register(bytes publicKey,address ecdsaOwner,bytes proof) payable returns (uint32)',
    'function execute(bytes records,bytes signatures,bool bls)'], validator);
assert.equal(await contract.count(), BigInt(expected.expectedAccounts.length));
for (const account of expected.expectedAccounts) {
  const derived = await contract.accounts(account.id);
  assert.equal(derived.publicKey, account.publicKey);
  assert.equal(derived.ecdsaOwner, account.ecdsaOwner);
  assert.equal(derived.nonce.toString(), account.nonce);
  assert.equal(derived.balance.toString(), account.balance);
  assert.equal((await validator.getBalance(account.recipient)).toString(), account.recipientBalance);
  // Read registration calldata from the derived chain: no external key/PoP
  // service or copy of the producer's state was supplied to the replay node.
  const registration = await validator.getTransaction(account.registrationHash);
  assert.equal(registration.to, expected.contract);
  const decoded = contract.interface.parseTransaction({ data: registration.data });
  assert.equal(decoded.name, 'register');
  assert.equal(decoded.args.publicKey, account.publicKey);
  assert.equal(decoded.args.ecdsaOwner, account.ecdsaOwner);
  assert.equal(decoded.args.proof, account.proof);
  assert.equal(getBytes(decoded.args.proof).length, 48);
}
const receipts = [...expected.setup.receipts,
  ...expected.rounds.flatMap(r => [r.baseline, r.alternative]), ...expected.workload.batches,
  expected.revertedTransaction, expected.revertedRegistration];
for (const receipt of receipts) {
  const derived = await validator.getTransactionReceipt(receipt.hash);
  assert(derived, `Missing derived transaction ${receipt.hash}`);
  assert.equal(derived.status, receipt.status);
  assert.equal(derived.blockNumber, receipt.blockNumber);
  if (receipt.blockHash) assert.equal(derived.blockHash, receipt.blockHash);
}
let derivedTransfers = 0;
for (const batch of expected.workload.batches) {
  const tx = await validator.getTransaction(batch.hash);
  assert.equal(tx.to, expected.contract);
  const decoded = contract.interface.parseTransaction({ data: tx.data });
  assert.equal(decoded.name, 'execute');
  assert.equal(decoded.args.bls, true);
  assert.equal(getBytes(decoded.args.signatures).length, 48);
  assert.equal(getBytes(decoded.args.records).length, batch.transfers * 44);
  derivedTransfers += batch.transfers;
}
assert.equal(derivedTransfers, expected.workload.transfers);

// Independent evidence that the local batcher actually used blob DA.
const blobTransactions = [];
const l1Head = await l1.getBlockNumber();
for (let height = 0; height <= l1Head; height++) {
  const b = await l1.send('eth_getBlockByNumber', [toBeHex(height), true]);
  for (const tx of b.transactions) {
    if (Number(tx.type) === 3) {
      const receipt = await l1.getTransactionReceipt(tx.hash);
      assert.equal(receipt.status, 1);
      blobTransactions.push({ hash: tx.hash, blockNumber: height, to: tx.to,
        blobVersionedHashes: tx.blobVersionedHashes });
    }
  }
}
assert(blobTransactions.length > 0, 'No L1 blob transactions observed');
const report = { result: 'PASS', validatorClient: await validator.send('web3_clientVersion', []),
  checkpoint: expected.checkpoint, safeHead: { number: safe.number, hash: safe.hash },
  executionPeers: 0, sequencerActive: active.result, checkedAccounts: expected.expectedAccounts.length,
  checkedRegistrationProofs: expected.expectedAccounts.length,
  derivedBatches: expected.workload.batches.length, derivedTransfers, aggregateBytesPerBatch: 48,
  checkedReceipts: receipts.length, l1Head, blobTransactions,
  networkPrecondition: 'Validator started from an empty datadir on compose.yaml internal l1-only network; no sequencer connectivity or state copying.',
  limitations: 'Development L1, not production finality or a formal security audit. Blob presence plus isolated safe derivation verifies DA availability; no per-bundle live blob-cost attribution.' };
const json = JSON.stringify(report, null, 2);
if (process.argv[3]) writeFileSync(process.argv[3], `${json}\n`);
console.log(json);
validator.destroy();
l1.destroy();
