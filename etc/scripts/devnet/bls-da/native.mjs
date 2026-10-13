// Full native flow: fund EOAs, register their BLS keys with PoPs, submit type
// 0x7a batches through the txpool, then export state for independent L1 replay.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';
import { brotliCompressSync, brotliDecompressSync, constants } from 'node:zlib';
import { HDNodeWallet, JsonRpcProvider, Wallet, concat, encodeRlp, getBytes, hexlify,
  keccak256, toQuantity, toUtf8Bytes } from 'ethers';
import { CHAIN_ID, DST, POP, TYPE, REGISTRY, bls, batch, registration,
  signatureBytes, signNative, decodeNative, decodePayload, rawFromRpc, rlpInt } from './native-codec.mjs';

const url = process.env.RPC_URL ?? 'http://127.0.0.1:7545';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(url).hostname));
const provider = new JsonRpcProvider(url, undefined, { cacheTimeout: -1 });
assert.equal((await provider.getNetwork()).chainId, CHAIN_ID);
assert.equal(await provider.getCode(REGISTRY), '0x', 'Native registry must not overlap a genesis predeploy');
const faucet = HDNodeWallet.fromPhrase('test test test test test test test test test test test junk').connect(provider);
assert(await provider.getBalance(faucet.address) > 40n * 10n ** 18n, 'Disposable faucet must be funded');
const runId = await provider.getTransactionCount(faucet.address);
const report = { client: await provider.send('web3_clientVersion', []), chainId: CHAIN_ID.toString(),
  type: TYPE, scheme: DST, proofScheme: POP, registry: REGISTRY,
  binarySha256: createHash('sha256').update(readFileSync(process.env.NATIVE_BASE_BINARY ??
    new URL('../../../../target/debug/base', import.meta.url))).digest('hex'),
  checks: [], receipts: [], registrations: [], batches: [], rejected: [] };
const pass = name => { report.checks.push(name); console.error(`PASS ${name}`); };

// Normalized singular-batch channels, not attribution from the live L1 blobs.
// Keep block boundaries and identical origin/parent metadata in both alternatives.
function channel(blocks) {
  const bytes = getBytes(concat(blocks.map((raws, i) => encodeRlp(concat(['0x00', encodeRlp([
    keccak256(toUtf8Bytes(`normalized parent ${i}`)), rlpInt(100),
    keccak256(toUtf8Bytes('normalized epoch')), rlpInt(1_800_000_000 + i * 2), raws,
  ])])))));
  const compressed = brotliCompressSync(bytes, { params: {
    [constants.BROTLI_PARAM_QUALITY]: 9, [constants.BROTLI_PARAM_LGWIN]: 22,
  } });
  assert.deepEqual(brotliDecompressSync(compressed), Buffer.from(bytes));
  return { uncompressed: bytes.length, compressed: compressed.length + 1 };
}

// Keep native responses raw: ethers' serializer does not know type 0x7a.
async function included(hash, raw, native = true) {
  const deadline = Date.now() + 120_000;
  let receipt;
  while (Date.now() < deadline) {
    receipt = await provider.send('eth_getTransactionReceipt', [hash]);
    if (receipt) break;
    await sleep(200);
  }
  assert(receipt && BigInt(receipt.status) === 1n, `Missing or failed receipt: ${hash}`);
  const block = await provider.send('eth_getBlockByNumber', [receipt.blockNumber, true]);
  assert.equal(block.hash, receipt.blockHash);
  const tx = block.transactions.find(t => t.hash === hash);
  assert(tx, 'Receipt must correspond to a transaction in the canonical block');
  if (native) {
    assert.equal(BigInt(receipt.type), 122n);
    assert.equal(rawFromRpc(tx), raw);
    const queried = await provider.send('eth_getTransactionByHash', [hash]);
    assert.equal(rawFromRpc(queried), raw);
    assert.equal(decodeNative(raw).hash, hash);
  }
  const saved = { hash, raw, blockNumber: receipt.blockNumber, blockHash: receipt.blockHash,
    status: receipt.status, type: receipt.type, gasUsed: receipt.gasUsed,
    l1Fee: receipt.l1Fee, effectiveGasPrice: receipt.effectiveGasPrice };
  report.receipts.push(saved);
  return saved;
}

async function sendNative(wallet, input) {
  const nonce = await provider.getTransactionCount(wallet.address);
  const estimatedGas = BigInt(await provider.send('eth_estimateGas', [{
    type: TYPE, chainId: toQuantity(CHAIN_ID), from: wallet.address, to: REGISTRY,
    value: '0x0', nonce: toQuantity(nonce), input, gas: toQuantity(5_500_000n),
    maxPriorityFeePerGas: toQuantity(1_000_000n), maxFeePerGas: toQuantity(100_000_000_000n),
  }]));
  const payload = decodePayload(input);
  const gasLimit = payload.kind === 'batch' ? 21_000n * BigInt(payload.records.length) : estimatedGas;
  assert(estimatedGas >= gasLimit, 'Gas estimation must cover the native execution price');
  const raw = signNative(wallet, input, { nonce, gasLimit });
  const before = await provider.getBalance(wallet.address);
  const hash = await provider.send('eth_sendRawTransaction', [raw]);
  assert.equal(hash, keccak256(raw));
  const receipt = await included(hash, raw);
  assert(BigInt(receipt.gasUsed) <= gasLimit, 'Native RPC gas estimation must cover execution');
  if (payload.kind === 'batch') assert.equal(BigInt(receipt.gasUsed), gasLimit);
  receipt.estimatedGas = estimatedGas.toString();
  assert.equal(await provider.getTransactionCount(wallet.address), nonce + 1);
  const feePaid = before - await provider.getBalance(wallet.address);
  assert(feePaid > 0n, 'Native fee payer must pay, not execute for free');
  receipt.feePaid = feePaid.toString();
  return receipt;
}

async function rejected(name, wallet, input, overrides = {}) {
  const nonce = await provider.getTransactionCount(wallet.address);
  const before = await provider.getBalance(wallet.address);
  const raw = signNative(wallet, input, { nonce, ...overrides });
  let failure;
  try { await provider.send('eth_sendRawTransaction', [raw]); }
  catch (error) {
    // Transport failures or a missing RPC method are not authorization rejection.
    const rpcError = error.info?.error ?? error.error;
    assert(rpcError && rpcError.code !== -32601, `Not a transaction rejection: ${error}`);
    failure = rpcError.message;
  }
  assert(failure, `${name}: invalid native transaction was admitted`);
  assert.equal(await provider.getTransactionCount(wallet.address), nonce);
  assert.equal(await provider.getBalance(wallet.address), before);
  assert.equal(await provider.send('eth_getTransactionReceipt', [keccak256(raw)]), null);
  report.rejected.push({ name, hash: keccak256(raw), reason: failure });
  pass(name);
}

const users = Array.from({ length: 32 }, (_, i) => {
  const key = bls.keygen(createHash('sha384').update(`PUBLIC NATIVE BLS ${runId}:${i}`).digest());
  return { ...key,
    owner: new Wallet(keccak256(toUtf8Bytes(`PUBLIC NATIVE ECDSA ${runId}:${i}`))),
    recipient: `0x${keccak256(toUtf8Bytes(`native recipient ${runId}:${i}`)).slice(-40)}`,
    value: BigInt(i + 1) * 1000n, sent: 0n, nonce: 0n };
}).sort((a, b) => a.owner.address.toLowerCase().localeCompare(b.owner.address.toLowerCase()));

for (const user of users) {
  const fund = await faucet.sendTransaction({ to: user.owner.address, value: 10n ** 18n });
  await included(fund.hash, undefined, false);
  const recipient = await faucet.sendTransaction({ to: user.recipient, value: 1n });
  await included(recipient.hash, undefined, false);
}
pass('Ordinary ECDSA transfers still fund native sender and recipient EOAs');

const first = users[0], firstRegistration = registration(first);
const wrongProof = registration(users[1]).proof;
await rejected('wrong-key PoP rejected without fee/nonce changes', first.owner,
  concat(['0x00', firstRegistration.publicKey, wrongProof]));
const wrongDomain = signatureBytes(bls.sign(bls.hash(getBytes(firstRegistration.publicKey), DST), first.secretKey));
await rejected('transaction-domain signature rejected as PoP', first.owner,
  concat(['0x00', firstRegistration.publicKey, wrongDomain]));
await rejected('missing PoP rejected', first.owner, concat(['0x00', firstRegistration.publicKey]));
await rejected('non-subgroup PoP rejected', first.owner,
  concat(['0x00', firstRegistration.publicKey, `0x80${'00'.repeat(47)}`]));
await rejected('foreign outer chain ID rejected', first.owner, firstRegistration.input, { chainId: 1n });
await rejected('unregistered sender batch rejected', faucet, batch(users.slice(0, 1)).input);

const attacker = bls.keygen(createHash('sha384').update('PUBLIC NATIVE ROGUE KEY').digest());
const rogue = attacker.publicKey.subtract(first.publicKey);
rogue.assertValidity();
const forgedPop = signatureBytes(bls.sign(bls.hash(rogue.toBytes(true), POP), attacker.secretKey));
await rejected('rogue key rejected by native PoP verification', first.owner,
  concat(['0x00', rogue.toBytes(true), forgedPop]));

for (const user of users) {
  const reg = registration(user);
  const receipt = await sendNative(user.owner, reg.input);
  user.nonce = 1n;
  user.startBalance = await provider.getBalance(user.owner.address);
  report.registrations.push({ owner: user.owner.address, publicKey: reg.publicKey, proof: reg.proof, hash: receipt.hash });
}
pass('32 native registrations included; PoPs published once; native nonces consumed');
await rejected('registered key cannot be replaced', first.owner, firstRegistration.input);

const valid = batch(users);
const altered = getBytes(valid.input); altered[61] ^= 1;
await rejected('altered inner transfer rejected', faucet, hexlify(altered));
await rejected('missing inner signer rejected', faucet,
  concat(['0x0120', ...valid.records, batch(users.slice(0, 31)).signature]));
await rejected('wrong inner chain domain rejected', faucet, batch(users, { chainId: CHAIN_ID + 1n }).input);
await rejected('duplicate sender rejected', faucet, batch([first, first]).input);
await rejected('insufficient native sender balance rejected', faucet, batch([first], { value: 10n ** 20n }).input);
await rejected('contract recipient unsupported rather than silently skipping code', faucet,
  batch([first], { recipient: '0x4200000000000000000000000000000000000015' }).input);

let lastBatch;
const nativeBlocks = [], directBlocks = [];
for (let offset = 0; offset < 1_000;) {
  // Include the one-transfer boundary: native gas is below ordinary calldata intrinsic gas.
  const members = users.slice(0, offset === 0 ? 1 : Math.min(32, 1_000 - offset));
  const b = batch(members);
  const direct = await Promise.all(members.map(user => user.owner.signTransaction({
    type: 2, chainId: CHAIN_ID, nonce: user.nonce, to: user.recipient, value: user.value,
    gasLimit: 21_000n, maxPriorityFeePerGas: 1_000_000n, maxFeePerGas: 100_000_000_000n,
  })));
  const receipt = await sendNative(faucet, b.input);
  nativeBlocks.push([receipt.raw]);
  directBlocks.push(direct);
  assert.equal(getBytes(decodePayload(decodeNative(receipt.raw).input).signature).length, 48);
  for (const user of members) {
    user.nonce++;
    user.sent += user.value;
  }
  report.batches.push({ hash: receipt.hash, transfers: members.length, aggregateBytes: 48 });
  console.error(`INCLUDED native batch ${report.batches.length}: ${members.length} transfers at ${receipt.blockNumber}`);
  lastBatch = b;
  offset += members.length;
}
await rejected('inner replay with fresh outer nonce rejected', faucet, lastBatch.input);

const nativeChannel = channel(nativeBlocks), directChannel = channel(directBlocks);
const registrationBlocks = report.registrations.map(reg => [report.receipts.find(r => r.hash === reg.hash).raw]);
report.daEstimate = {
  transfers: 1_000, registeredSenders: 32, existingRecipients: 32,
  nativeChannel, directEoaChannel: directChannel,
  nativeRegistrationChannel: channel(registrationBlocks),
  nativeIncludingRegistration: channel([...registrationBlocks, ...nativeBlocks]),
  warmThroughputMultiplier: directChannel.compressed / nativeChannel.compressed,
  warmSavingsPercent: 100 * (1 - nativeChannel.compressed / directChannel.compressed),
  perBatchFlushNativeBytes: nativeBlocks.reduce((n, block) => n + channel([block]).compressed, 0),
  perBatchFlushDirectBytes: directBlocks.reduce((n, block) => n + channel([block]).compressed, 0),
  nativeExecutionGas: report.batches.reduce((n, b) => n + BigInt(report.receipts.find(r => r.hash === b.hash).gasUsed), 0n).toString(),
  directEoaExecutionGas: '21000000',
  scope: 'Native envelopes were mined at 21,000 execution gas per transfer; equally signed ECDSA references were not broadcast. Normalized singular channels preserve batch boundaries, use Brotli quality 9, and exclude blob padding. Repeated 32 sender/recipient pairs and small amounts are unusually compressible; this is not a forecast for 1,000 distinct recipients or Base-wide traffic. Cold figures include native registration envelopes, keys, PoPs, and ECDSA migration authorization, but exclude common funding transfers. L1 data and operator fees remain separate.',
};

report.expectedAccounts = [];
for (const user of users) {
  const balance = await provider.getBalance(user.owner.address);
  const recipientBalance = await provider.getBalance(user.recipient);
  const nonce = await provider.getTransactionCount(user.owner.address);
  assert.equal(balance, user.startBalance - user.sent);
  assert.equal(recipientBalance, 1n + user.sent);
  assert.equal(BigInt(nonce), user.nonce);
  assert.equal(await provider.getCode(user.owner.address), '0x');
  report.expectedAccounts.push({ address: user.owner.address, nonce, balance: balance.toString(),
    recipient: user.recipient, recipientBalance: recipientBalance.toString() });
}
pass('1,000 native transfers: exact EOA balances/nonces, separate payer, no deployed vault');
for (const denied of report.rejected) {
  assert.equal(await provider.send('eth_getTransactionReceipt', [denied.hash]), null);
}
const checkpoint = await provider.send('eth_getBlockByNumber', [report.receipts.at(-1).blockNumber, false]);
report.checkpoint = Object.fromEntries(['number', 'hash', 'stateRoot', 'transactionsRoot', 'receiptsRoot'].map(k => [k, checkpoint[k]]));
report.scope = 'Native type0x7a execution/RPC/txpool, development chain only; independent L1 derivation must be checked with verify-native.mjs. Not a production fork or discounted fee rule.';
const json = JSON.stringify(report, null, 2);
if (process.argv[2]) writeFileSync(process.argv[2], `${json}\n`);
console.log(json);
provider.destroy();
