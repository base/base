// Fresh, disposable development chain only. Users call create()/transfer(); the
// rest of this file is a checker inspecting what the sequencer/batcher produced.
import assert from 'node:assert/strict';
import { createHash, createHmac } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';
import { brotliCompressSync, brotliDecompressSync, constants } from 'node:zlib';
import { HDNodeWallet, IpcSocketProvider, JsonRpcProvider, Wallet, concat, decodeRlp, encodeRlp, getBytes,
  hexlify, keccak256, toQuantity, toUtf8Bytes } from 'ethers';
import { CHAIN_ID, DST, POP, REGISTRY, bls, registration, rawFromRpc, transferFromRpc,
  signTransfer, signatureBytes, rlpInt } from './native-codec.mjs';
import { NativeBlsWallet } from './intent-wallet.mjs';

const url = process.env.RPC_URL ?? 'http://127.0.0.1:7545';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(url).hostname));
const provider = new JsonRpcProvider(url, undefined, { cacheTimeout: -1 });
assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
assert.equal(await provider.getCode(REGISTRY), '0x');
const engineIpc = process.env.ENGINE_IPC_PATH ? new IpcSocketProvider(process.env.ENGINE_IPC_PATH) : undefined;
if (engineIpc) await engineIpc.send('engine_exchangeCapabilities', [[]]);
if (process.env.ENGINE_RPC_URL) {
  assert(process.env.ENGINE_JWT_FILE, 'Engine checks require ENGINE_JWT_FILE');
  // Fail before funding accounts if the container's authenticated port is unreachable.
  const response = await fetch(process.env.ENGINE_RPC_URL, { signal: AbortSignal.timeout(10_000) });
  assert.equal(response.status, 401, 'Engine endpoint must require JWT authentication');
}
const faucet = HDNodeWallet.fromPhrase('test test test test test test test test test test test junk').connect(provider);
const run = await provider.getTransactionCount(faucet.address);
const rounds = Number(process.env.ROUNDS ?? 130);
assert(Number.isSafeInteger(rounds) && rounds > 0);
const report = { chainId: CHAIN_ID.toString(), client: await provider.send('web3_clientVersion', []),
  binarySha256: createHash('sha256').update(readFileSync(process.env.NATIVE_BASE_BINARY ??
    new URL('../../../../target/debug/base', import.meta.url))).digest('hex'),
  checks: [], registrations: [], receipts: [], blocks: [], rejected: [] };
const pass = name => { report.checks.push(name); console.error(`PASS ${name}`); };

async function included(hash) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const receipt = await provider.send('eth_getTransactionReceipt', [hash]);
    if (receipt) {
      assert.equal(BigInt(receipt.status), 1n);
      report.receipts.push(receipt);
      return receipt;
    }
    await sleep(200);
  }
  assert.fail(`No successful receipt for ${hash}`);
}

const users = Array.from({ length: 8 }, (_, i) => {
  const key = bls.keygen(createHash('sha384').update(`PUBLIC INTENT BLS ${run}:${i}`).digest());
  const owner = new Wallet(keccak256(toUtf8Bytes(`PUBLIC INTENT ECDSA ${run}:${i}`)));
  return { ...key, owner, wallet: new NativeBlsWallet(provider, owner, key), sent: 0n,
    recipient: `0x${keccak256(toUtf8Bytes(`intent recipient ${run}:${i}`)).slice(-40)}`,
    value: BigInt(i + 1) * 1001n };
});
for (const user of users) {
  await included((await faucet.sendTransaction({ to: user.owner.address, value: 10n ** 18n })).hash);
  await included((await faucet.sendTransaction({ to: user.recipient, value: 1n })).hash);
}
pass('ordinary ECDSA funding still executes');
for (const user of users) {
  const hash = await user.wallet.create();
  await included(hash);
  const tx = await provider.send('eth_getTransactionByHash', [hash]);
  const reg = registration(user);
  report.registrations.push({ owner: user.owner.address, hash, raw: rawFromRpc(tx),
    publicKey: reg.publicKey, proof: reg.proof });
  user.startBalance = await provider.getBalance(user.owner.address);
}
pass('create() publishes a one-time ECDSA-authorized key and proof of possession');

async function reject(name, raw, owner) {
  const balance = await provider.getBalance(owner.address);
  const nonce = await provider.getTransactionCount(owner.address);
  let denied;
  try { await provider.send('eth_sendRawTransaction', [raw]); }
  catch (error) {
    denied = error.info?.error ?? error.error;
    assert(denied && denied.code !== -32601, `Not a transaction rejection: ${error}`);
  }
  assert(denied, `${name} was accepted`);
  assert.equal(await provider.getBalance(owner.address), balance);
  assert.equal(await provider.getTransactionCount(owner.address), nonce);
  report.rejected.push({ name, reason: denied.message });
}

const first = users[0];
const intent = { chainId: CHAIN_ID, sender: first.owner.address, nonce: 1n,
  recipient: first.recipient, value: first.value,
  maxPriorityFeePerGas: 1_000_000n, maxFeePerGas: 100_000_000_000n };
const signed = signTransfer(intent, first.secretKey);
await reject('unsigned canonical intent', signed.canonical, first.owner);
await reject('wrong signing key', signTransfer(intent, users[1].secretKey).raw, first.owner);
await reject('changed recipient with original signature', concat(['0x7b', encodeRlp([
  signTransfer({ ...intent, recipient: users[1].recipient }, first.secretKey).fields, signed.signature,
])]), first.owner);
await reject('foreign chain', signTransfer({ ...intent, chainId: 1n }, first.secretKey).raw, first.owner);
await reject('infinite signature', concat(['0x7b', encodeRlp([signed.fields, `0xc0${'00'.repeat(47)}`])]), first.owner);
pass('unsigned, forged, altered, foreign-chain and infinite-point submissions rejected without debits');

const transferReceipts = [];
for (let round = 0; round < rounds; round++) {
  const hashes = await Promise.all(users.map(user => user.wallet.transfer({ to: user.recipient, value: user.value })));
  const receipts = await Promise.all(hashes.map(included));
  for (const receipt of receipts) {
    assert.equal(BigInt(receipt.type), 123n);
    assert.equal(BigInt(receipt.gasUsed), 21_000n);
    transferReceipts.push(receipt);
  }
  for (const user of users) user.sent += user.value;
  console.error(`INCLUDED ${round + 1}/${rounds}: ${users.length} independent transfers`);
}
await reject('replay of an included intent', signed.raw, first.owner);
pass(`transfer() sent ${transferReceipts.length} independent sender-paid intents, each using exactly 21,000 gas`);

const start = transferReceipts.reduce((n, r) => BigInt(r.blockNumber) < n ? BigInt(r.blockNumber) : n, 2n ** 64n);
const end = transferReceipts.reduce((n, r) => BigInt(r.blockNumber) > n ? BigInt(r.blockNumber) : n, 0n);
const keys = new Map(users.map(user => [user.owner.address.toLowerCase(), user]));
const referenceBlocks = [];
for (let height = start; height <= end; height++) {
  const block = await provider.send('eth_getBlockByNumber', [toQuantity(height), true]);
  const txs = block.transactions.filter(tx => BigInt(tx.type) === 123n);
  const aggregate = await provider.send('base_blsBlockAggregate', [block.hash]);
  assert.equal(aggregate !== null, txs.length > 0);
  const intents = txs.map(transferFromRpc);
  if (txs.length) {
    assert.equal(getBytes(aggregate).length, 48);
    assert(bls.verifyBatch(bls.Signature.fromBytes(getBytes(aggregate)), txs.map((tx, i) => ({
      message: bls.hash(intents[i].digest, DST), publicKey: keys.get((tx.sender ?? tx.from).toLowerCase()).publicKey,
    }))));
  }
  const direct = await Promise.all(txs.map(tx => keys.get((tx.sender ?? tx.from).toLowerCase()).owner.signTransaction({
    type: 2, chainId: CHAIN_ID, nonce: BigInt(tx.nonce), to: tx.recipient ?? tx.to, value: BigInt(tx.value),
    gasLimit: 21_000n, maxPriorityFeePerGas: BigInt(tx.maxPriorityFeePerGas), maxFeePerGas: BigInt(tx.maxFeePerGas),
  })));
  report.blocks.push({ number: block.number, hash: block.hash, parentHash: block.parentHash,
    timestamp: block.timestamp, aggregate, intents: intents.map(tx => tx.canonical) });
  referenceBlocks.push(direct);
}
pass('sequencer block witnesses verify independently with noble; canonical intents contain no signature');

// This diagnostic endpoint uses the same state-backed chunk verifier as L1 derivation.
const sample = report.blocks.find(block => block.intents.length);
assert.equal(await provider.send('base_verifyBlsChunk', [sample.parentHash, [sample.intents], sample.aggregate]), true);
assert.equal(await provider.send('base_verifyBlsChunk', [sample.parentHash, [sample.intents], null]), false);
assert.equal(await provider.send('base_verifyBlsChunk', [sample.parentHash, [sample.intents.slice(1)], sample.aggregate]), false);
const wrongAggregate = signatureBytes(bls.sign(bls.hash(getBytes(keccak256(toUtf8Bytes('wrong chunk'))), DST), first.secretKey));
assert.equal(await provider.send('base_verifyBlsChunk', [sample.parentHash, [sample.intents], wrongAggregate]), false);
pass('live state-backed chunk verifier rejects missing, omitted-member and wrong-message aggregates');

// Exercise the authenticated Engine boundary too, including the already-valid header cache.
if (engineIpc || (process.env.ENGINE_JWT_FILE && process.env.ENGINE_RPC_URL)) {
  const block = await provider.send('eth_getBlockByHash', [sample.hash, false]);
  const rawBlock = decodeRlp(await provider.send('debug_getRawBlock', [sample.hash]));
  const transactions = rawBlock[1].map(tx => Array.isArray(tx) ? encodeRlp(tx) : tx);
  const payload = { parentHash: block.parentHash, feeRecipient: block.miner, stateRoot: block.stateRoot,
    receiptsRoot: block.receiptsRoot, logsBloom: block.logsBloom, prevRandao: block.mixHash,
    blockNumber: block.number, gasLimit: block.gasLimit, gasUsed: block.gasUsed, timestamp: block.timestamp,
    extraData: block.extraData, baseFeePerGas: block.baseFeePerGas, blockHash: block.hash, transactions,
    withdrawals: [], blobGasUsed: block.blobGasUsed, excessBlobGas: block.excessBlobGas,
    withdrawalsRoot: block.withdrawalsRoot };
  const secret = engineIpc ? undefined : Buffer.from(readFileSync(process.env.ENGINE_JWT_FILE, 'utf8').trim().replace(/^0x/, ''), 'hex');
  async function engine(witness) {
    const params = [payload, [], block.parentBeaconBlockRoot, []];
    if (witness !== undefined) params.push(witness);
    if (engineIpc) {
      try { return { result: await engineIpc.send('engine_newPayloadV4', params) }; }
      catch (error) {
        const rpcError = error.error ?? error.info?.error;
        assert(rpcError && rpcError.code !== -32601, `Not an Engine rejection: ${error}`);
        return { error: rpcError };
      }
    }
    const body = [Buffer.from(JSON.stringify({ alg: 'HS256', typ: 'JWT' })).toString('base64url'),
      Buffer.from(JSON.stringify({ iat: Math.floor(Date.now() / 1000) })).toString('base64url')].join('.');
    const token = `${body}.${createHmac('sha256', secret).update(body).digest('base64url')}`;
    const response = await fetch(process.env.ENGINE_RPC_URL, { method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'engine_newPayloadV4', params }),
      signal: AbortSignal.timeout(30_000) });
    assert(response.ok);
    return response.json();
  }
  assert.equal((await engine({ aggregate: sample.aggregate })).result?.status, 'VALID');
  assert((await engine()).error, 'missing witness must not use cached VALID');
  assert((await engine({ aggregate: wrongAggregate })).error, 'forged witness must not use cached VALID');
  assert.equal((await engine({ aggregate: sample.aggregate })).result?.status, 'VALID');
  assert.equal(await provider.send('base_blsBlockAggregate', [sample.hash]), sample.aggregate);
  pass('Engine rejects missing/forged witnesses even for cached VALID; valid witness still succeeds and survives');
}

function compress(items) {
  const bytes = getBytes(concat(items.map(item => encodeRlp(item))));
  const compressed = brotliCompressSync(bytes, { params: {
    [constants.BROTLI_PARAM_QUALITY]: 9, [constants.BROTLI_PARAM_LGWIN]: 22,
  } });
  assert.deepEqual(brotliDecompressSync(compressed), Buffer.from(bytes));
  return compressed.length + 1;
}
const epoch = keccak256(toUtf8Bytes('normalized common epoch'));
const single = (block, txs) => [block.parentHash, rlpInt(100), epoch, rlpInt(block.timestamp), txs];
report.daModel = [];
for (const width of [32, 64, 128]) {
  const nativeItems = [], ecdsaItems = [];
  let nativeFlushBytes = 0, ecdsaFlushBytes = 0, signatures = 0;
  for (let offset = 0; offset < report.blocks.length; offset += width) {
    const blocks = report.blocks.slice(offset, offset + width);
    const points = blocks.flatMap(block => block.aggregate ? [bls.Signature.fromBytes(getBytes(block.aggregate))] : []);
    const aggregate = points.length ? signatureBytes(bls.aggregateSignatures(points)) : '0x';
    if (points.length) signatures++;
    const native = concat(['0x02', encodeRlp([aggregate, blocks.map(block => single(block, block.intents))])]);
    const ecdsa = blocks.map((block, i) => concat(['0x00', encodeRlp(single(block, referenceBlocks[offset + i]))]));
    nativeItems.push(native); ecdsaItems.push(...ecdsa);
    nativeFlushBytes += compress([native]); ecdsaFlushBytes += compress(ecdsa);
  }
  const nativeBytes = compress(nativeItems), ecdsaBytes = compress(ecdsaItems);
  report.daModel.push({ chunkBlocks: width, transfers: transferReceipts.length,
    aggregateBytes: signatures * 48, nativeBytes, ecdsaBytes,
    capacityMultiplier: ecdsaBytes / nativeBytes, savingsPercent: 100 * (1 - nativeBytes / ecdsaBytes),
    perChunkFlushNativeBytes: nativeFlushBytes, perChunkFlushEcdsaBytes: ecdsaFlushBytes });
}
report.registrationChannelBytes = compress(report.registrations.map((reg, i) => concat(['0x00', encodeRlp([
  keccak256(toUtf8Bytes(`registration parent ${i}`)), rlpInt(100), epoch, rlpInt(i * 2), [reg.raw],
])])));
report.daModelScope = 'Controlled Brotli-9 channel model using mined canonical intents and independently signed ECDSA counterparts. Same block boundaries/parent metadata and normalized common origin. Repeated eight sender/recipient pairs are unusually compressible. Excludes blob padding, common funding, and receipt/execution costs. Per-chunk flush is a sensitivity, not measured live blob utilization. Registration bytes include keys, PoPs, ECDSA authorization and envelopes; warm tables exclude that one-time cost. No Base-wide TPS claim.';

report.expectedAccounts = [];
for (const user of users) {
  const balance = await provider.getBalance(user.owner.address);
  const nonce = await provider.getTransactionCount(user.owner.address);
  const recipientBalance = await provider.getBalance(user.recipient);
  assert.equal(nonce, rounds + 1);
  assert.equal(recipientBalance, 1n + user.sent);
  assert(balance < user.startBalance - user.sent, 'Each sender must pay its own fees');
  report.expectedAccounts.push({ address: user.owner.address, nonce, balance: balance.toString(),
    recipient: user.recipient, recipientBalance: recipientBalance.toString(),
    feePaid: (user.startBalance - user.sent - balance).toString() });
}
const checkpoint = await provider.send('eth_getBlockByNumber', [toQuantity(end), false]);
report.checkpoint = Object.fromEntries(['number', 'hash', 'stateRoot', 'transactionsRoot', 'receiptsRoot'].map(k => [k, checkpoint[k]]));
report.transfers = transferReceipts.length;
report.executionGas = (21_000n * BigInt(transferReceipts.length)).toString();
report.scope = 'Development chain only; new native intent RPC/pool/execution and block witnesses. L1 reconstruction requires a separate fresh isolated node. Not a security audit, proof-system benchmark or production throughput result.';
pass('sender nonces, recipient balances and sender-paid fees match the independent transfers');
const json = JSON.stringify(report, null, 2);
if (process.argv[2]) writeFileSync(process.argv[2], `${json}\n`);
console.log(json);
provider.destroy();
engineIpc?.destroy();
