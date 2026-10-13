import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { brotliCompressSync, brotliDecompressSync, constants } from 'node:zlib';
import { bls12_381 as curve } from '@noble/curves/bls12-381.js';
import {
  ContractFactory, HDNodeWallet, JsonRpcProvider, NonceManager, Transaction, Wallet,
  concat, encodeRlp, getBytes, hexlify, keccak256, solidityPacked, toBeHex, toUtf8Bytes,
} from 'ethers';
import solc from 'solc';

// Public, disposable test keys only. Refuse non-loopback RPCs and public chain IDs.
const rpc = process.env.RPC_URL ?? 'http://127.0.0.1:7545';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(rpc).hostname));
const provider = new JsonRpcProvider(rpc, undefined, { cacheTimeout: -1 });
provider.pollingInterval = 100;
const chainId = (await provider.getNetwork()).chainId;
assert([1337n, 84538453n].includes(chainId), `Not a supported disposable chain: ${chainId}`);
const clientVersion = await provider.send('web3_clientVersion', []);
assert(/reth|base/i.test(clientVersion), `Expected Base execution client: ${clientVersion}`);
const faucet = HDNodeWallet.fromPhrase('test test test test test test test test test test test junk').connect(provider);
assert(await provider.getBalance(faucet.address) > 10n ** 18n, 'Fund the public test account on the disposable devnet');
const relayer = new NonceManager(faucet);
const bls = curve.shortSignatures;
const DST = 'BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_';
const POP = 'BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_';
const DOMAIN = keccak256(toUtf8Bytes('base-bls-da-example:v2:eth-transfer'));
const GAS_LIMIT = 5_500_000; // Leave room for system deposits in a 6M-gas Denim block.
const fp = x => toBeHex(x, 64);
const g1 = point => { const { x, y } = point.toAffine(); return concat([fp(x), fp(y)]); };
const g2 = point => {
  const { x, y } = point.toAffine();
  return concat([fp(x.c0), fp(x.c1), fp(y.c0), fp(y.c1)]);
};
const sigBytes = point => hexlify(bls.Signature.toBytes(point));
const report = { clientVersion, chainId: chainId.toString(), solc: solc.version(),
  scheme: DST, proofScheme: POP, cases: [], rounds: [] };
const pass = name => { report.cases.push(name); console.error(`PASS ${name}`); };

// Establish that the actual client has a working subgroup-checking G2 precompile.
assert.equal(await provider.call({ to: toBeHex(14, 20), data: concat([g2(curve.G2.Point.BASE), toBeHex(1, 32)]) }), g2(curve.G2.Point.BASE));
pass('Base EIP-2537 G2 precompile available');

const source = readFileSync(new URL('./BLSDAExample.sol', import.meta.url), 'utf8');
const output = JSON.parse(solc.compile(JSON.stringify({
  language: 'Solidity', sources: { 'BLSDAExample.sol': { content: source } },
  settings: { optimizer: { enabled: true, runs: 200 }, viaIR: true, evmVersion: 'cancun',
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } },
})));
assert(!(output.errors ?? []).some(e => e.severity === 'error'), JSON.stringify(output.errors));
const compiled = output.contracts['BLSDAExample.sol'].BLSDAExample;
const contract = await new ContractFactory(compiled.abi, compiled.evm.bytecode.object, relayer).deploy();
await receipt(contract.deploymentTransaction());
const address = await contract.getAddress();
report.contract = address;
report.sourceHash = keccak256(toUtf8Bytes(source));
report.runtimeHash = keccak256(await provider.getCode(address));

async function receipt(tx) {
  const result = await provider.waitForTransaction(tx.hash, 1, 60_000);
  assert(result && result.status === 1, `Transaction failed: ${tx.hash}`);
  const block = await provider.getBlock(result.blockNumber);
  assert.equal(block.hash, result.blockHash);
  assert(block.transactions.includes(tx.hash));
  return { hash: tx.hash, blockNumber: result.blockNumber, blockHash: result.blockHash,
    status: result.status, gasUsed: result.gasUsed.toString() };
}

async function rejected(name, call, reason) {
  await assert.rejects(call, error => error.code === 'CALL_EXCEPTION' && error.reason === reason);
  pass(name);
}

// Validate both compressed sign-bit branches and RFC 9380 hashing independently
// against noble before relying on successful signature verification.
for (const point of [curve.G1.Point.BASE, curve.G1.Point.BASE.negate()]) {
  assert.equal(await contract.decompress(sigBytes(point)), g1(point));
}
for (const message of ['0x', '0x1234', hexlify(new Uint8Array(80).fill(0xa5))]) {
  for (const dst of [DST, POP]) {
    assert.equal(await contract.hashToG1(message, toUtf8Bytes(dst)), g1(bls.hash(getBytes(message), dst)));
  }
}
pass('G1 decompression (both signs) and both hash-to-curve domains agree with noble');

await rejected('identity public key rejected', () => contract.register.staticCall(new Uint8Array(256), faucet.address, '0x'), 'identity key');
const invalidKey = getBytes(g2(curve.G2.Point.BASE));
invalidKey[0] = 1;
await rejected('noncanonical public key rejected', () => contract.register.staticCall(invalidKey, faucet.address, '0x'), 'precompile');

const setupTxs = [await provider.getTransaction(contract.deploymentTransaction().hash)];
const setupReceipts = [await receipt(contract.deploymentTransaction())];
const users = [];
for (let i = 0; i < 32; i++) {
  const key = bls.keygen(createHash('sha384').update(`PUBLIC BLS DEMO KEY ${i}`).digest());
  const owner = new Wallet(keccak256(toUtf8Bytes(`PUBLIC ECDSA DEMO KEY ${i}`)));
  const recipient = `0x${keccak256(toUtf8Bytes(`BLS DEMO recipient ${address} ${i}`)).slice(-40)}`;
  const proof = sigBytes(bls.sign(bls.hash(key.publicKey.toBytes(true), POP), key.secretKey));
  const tx = await contract.register(g2(key.publicKey), owner.address, proof, { value: 10n ** 16n });
  setupReceipts.push(await receipt(tx));
  setupTxs.push(await provider.getTransaction(tx.hash));
  users.push({ ...key, owner, recipient, proof, registrationHash: tx.hash,
    id: i + 1, nonce: 0n, value: BigInt(i + 1) * 1000n });
}
assert.equal(await contract.count(), 32n);
// Both G2 sort-bit branches must have passed the contract's canonical encoding.
assert.equal(new Set(users.map(u => u.publicKey.toBytes(true)[0] & 0x20)).size, 2);
pass('32 funded immutable BLS keys registered with on-chain PoP validation (both key signs)');
await rejected('missing PoP rejected', () => contract.register.staticCall(g2(users[0].publicKey), faucet.address, '0x'), 'G1 length');
await rejected('another key\'s PoP rejected', () => contract.register.staticCall(g2(users[0].publicKey), faucet.address, users[1].proof), 'BLS proof of possession');
const wrongDomainProof = sigBytes(bls.sign(bls.hash(users[0].publicKey.toBytes(true), DST), users[0].secretKey));
await rejected('transaction-domain signature rejected as PoP', () => contract.register.staticCall(g2(users[0].publicKey), faucet.address, wrongDomainProof), 'BLS proof of possession');
await rejected('non-subgroup PoP rejected', () => contract.register.staticCall(g2(users[0].publicKey), faucet.address, `0x80${'00'.repeat(47)}`), 'precompile');

function bundle(members, overrides = {}) {
  const records = members.map(user => solidityPacked(['uint32', 'uint64', 'address', 'uint96'],
    [user.id, user.nonce, user.recipient, user.value]));
  const messages = records.map(record => getBytes(keccak256(solidityPacked(['bytes32', 'uint256', 'address', 'bytes'],
    [DOMAIN, overrides.chainId ?? chainId, overrides.address ?? address, record]))));
  const aggregate = bls.aggregateSignatures(members.map((user, i) => bls.sign(bls.hash(messages[i], DST), user.secretKey)));
  assert(bls.verifyBatch(aggregate, members.map((user, i) => ({ message: bls.hash(messages[i], DST), publicKey: user.publicKey }))));
  return { records: concat(records), bls: sigBytes(aggregate),
    ecdsa: concat(members.map((user, i) => user.owner.signingKey.sign(messages[i]).serialized)) };
}

// Source-compatible SingleBatch wire model: RLP(type || RLP(fields)), then
// Brotli quality 9 (base-comp default), lgwin 22, prefixed by channel version 1. Fixed synthetic
// metadata isolates authorization; this is NOT an actual L1-posted channel.
const integerRlp = n => n === 0 ? '0x' : toBeHex(n);
function channel(rawTxs) {
  const batch = encodeRlp(concat(['0x00', encodeRlp([
    keccak256(toUtf8Bytes('fixed parent')), integerRlp(100),
    keccak256(toUtf8Bytes('fixed epoch')), integerRlp(1_800_000_000), rawTxs,
  ])]));
  const input = getBytes(batch);
  const compressed = brotliCompressSync(input, { params: {
    [constants.BROTLI_PARAM_QUALITY]: 9, [constants.BROTLI_PARAM_LGWIN]: 22,
  } });
  assert.deepEqual(brotliDecompressSync(compressed), Buffer.from(input));
  return { uncompressed: input.length, compressed: 1 + compressed.length };
}
async function signedEnvelope(data, overrides = {}) {
  return faucet.signTransaction({ type: 2, chainId, to: address, data, nonce: 100,
    gasLimit: GAS_LIMIT, maxFeePerGas: 1_000_000_000, maxPriorityFeePerGas: 1_000_000, ...overrides });
}

for (const n of [1, 8, 32]) {
  const members = users.slice(0, n);
  let b = bundle(members);
  const before = await Promise.all(members.map(u => provider.getBalance(u.recipient)));
  const baseline = await receipt(await contract.execute(b.records, b.ecdsa, false, { gasLimit: GAS_LIMIT }));
  members.forEach(u => u.nonce++);
  b = bundle(members);
  // Both modes authorize the exact same current records before the BLS send.
  await contract.execute.staticCall(b.records, b.ecdsa, false);
  await contract.execute.staticCall(b.records, b.bls, true);
  const sameStateGasEstimate = {
    ecdsa: (await contract.execute.estimateGas(b.records, b.ecdsa, false)).toString(),
    bls: (await contract.execute.estimateGas(b.records, b.bls, true)).toString(),
  };
  const ecdsaRaw = await signedEnvelope(contract.interface.encodeFunctionData('execute', [b.records, b.ecdsa, false]));
  const blsRaw = await signedEnvelope(contract.interface.encodeFunctionData('execute', [b.records, b.bls, true]));
  // Ordinary EOA transfers need neither a key registry nor explicit sender IDs.
  // These signed reference transactions are size measurements, not broadcast.
  const directRaw = await Promise.all(members.map(user => user.owner.signTransaction({
    type: 2, chainId, to: user.recipient, value: user.value, nonce: Number(user.nonce),
    gasLimit: 21_000, maxFeePerGas: 1_000_000_000, maxPriorityFeePerGas: 1_000_000,
  })));
  const alternative = await receipt(await contract.execute(b.records, b.bls, true, { gasLimit: GAS_LIMIT }));
  members.forEach(u => u.nonce++);
  for (const [i, user] of members.entries()) {
    assert.equal(await provider.getBalance(user.recipient), before[i] + 2n * user.value);
    const account = await contract.accounts(user.id);
    assert.equal(account.nonce, user.nonce);
    assert.equal(account.balance, 10n ** 16n - user.nonce * user.value);
  }
  const ec = channel([ecdsaRaw]), bs = channel([blsRaw]), direct = channel(directRaw);
  report.rounds.push({ n, signatureBytes: { ecdsa: 65 * n, bls: getBytes(b.bls).length },
    normalizedChannelEstimate: { ecdsa: ec, bls: bs, directEoa: direct,
      savingsPercent: 100 * (ec.compressed - bs.compressed) / ec.compressed,
      savingsVsDirectPercent: 100 * (direct.compressed - bs.compressed) / direct.compressed },
    sameStateGasEstimate, baseline, alternative });
  pass(`${n} ECDSA + ${n} BLS transfers included; balances and nonces exact`);
  await rejected(`${n}-transfer replay rejected`, () => contract.execute.staticCall(b.records, b.bls, true), 'nonce');
}

// Recipients now exist from the control rounds. Execute the previously modeled
// 1,000-transfer workload for real: 31 full batches and one eight-transfer batch.
const workloadBefore = await Promise.all(users.map(u => provider.getBalance(u.recipient)));
const sent = users.map(() => 0n);
const workloadRaw = [], directWorkloadRaw = [], batches = [];
for (let offset = 0; offset < 1_000; offset += 32) {
  const members = users.slice(0, Math.min(32, 1_000 - offset));
  const b = bundle(members);
  assert.equal(getBytes(b.bls).length, 48);
  for (const user of members) {
    directWorkloadRaw.push(await user.owner.signTransaction({ type: 2, chainId,
      to: user.recipient, value: user.value, nonce: Number(user.nonce), gasLimit: 21_000,
      maxFeePerGas: 1_000_000_000, maxPriorityFeePerGas: 1_000_000 }));
  }
  const tx = await contract.execute(b.records, b.bls, true, { gasLimit: GAS_LIMIT });
  const included = await receipt(tx);
  const raw = Transaction.from(await provider.getTransaction(tx.hash)).serialized;
  assert.equal(keccak256(raw), tx.hash);
  workloadRaw.push(raw);
  batches.push({ transfers: members.length, aggregateBytes: 48, ...included });
  for (const user of members) {
    user.nonce++;
    sent[user.id - 1] += user.value;
  }
  console.error(`INCLUDED PoP batch ${batches.length}: ${members.length} transfers, 48-byte aggregate, block ${included.blockNumber}`);
}
for (const [i, user] of users.entries()) {
  const account = await contract.accounts(user.id);
  assert.equal(account.nonce, user.nonce);
  assert.equal(account.balance, 10n ** 16n - user.nonce * user.value);
  assert.equal(await provider.getBalance(user.recipient), workloadBefore[i] + sent[i]);
}
assert.equal(await contract.count(), 32n, 'Workload must not register more keys');
report.workload = { transfers: 1_000, batches, newRegistrations: 0,
  aggregateSignatureBytes: batches.length * 48,
  gasUsed: batches.reduce((sum, b) => sum + BigInt(b.gasUsed), 0n).toString(),
  normalizedChannelEstimate: { bls: channel(workloadRaw), directEoa: channel(directWorkloadRaw) },
  sizingScope: 'One synthetic SingleBatch packs all envelopes to isolate compression; not actual block packing or live blob-byte attribution. BLS envelopes were mined; direct EOA references were not broadcast.' };
pass('1,000 transfers to existing recipients in 32 batches; one aggregate each; no new registrations; exact final state');

const members = users.slice(0, 8);
const valid = bundle(members);
await rejected('altered transfer amount rejected', () => {
  const altered = getBytes(valid.records); altered[43] ^= 1;
  return contract.execute.staticCall(altered, valid.bls, true);
}, 'BLS signature');
for (const [name, overrides] of [
  ['wrong chain domain rejected', { chainId: chainId + 1n }],
  ['wrong contract domain rejected', { address: faucet.address }],
]) {
  const b = bundle(members, overrides);
  await rejected(name, () => contract.execute.staticCall(b.records, b.bls, true), 'BLS signature');
}
await rejected('missing signer rejected', () => contract.execute.staticCall(valid.records, bundle(members.slice(0, -1)).bls, true), 'BLS signature');
await rejected('duplicate account/message rejected', () => {
  const b = bundle([members[0], members[0]]);
  return contract.execute.staticCall(b.records, b.bls, true);
}, 'distinct ordered accounts');
await rejected('identity signature rejected', () => contract.execute.staticCall(valid.records, `0xc0${'00'.repeat(47)}`, true), 'G1 flags/infinity');
await rejected('truncated signature rejected', () => contract.execute.staticCall(valid.records, getBytes(valid.bls).slice(1), true), 'signature length');
await rejected('non-subgroup G1 signature rejected', () => contract.execute.staticCall(valid.records, `0x80${'00'.repeat(47)}`, true), 'precompile');
// G1 point (0,2) is on the curve but outside its prime-order subgroup.

// Broadcast a real invalid transaction too; a failed eth_call alone is not proof
// that inclusion rolls back the contract's early nonce/balance writes.
const badRecord = getBytes(valid.records); badRecord[43] ^= 1;
const beforeBad = await Promise.all(members.map(u => contract.accounts(u.id)));
const beforeBadBalances = await Promise.all(members.map(u => provider.getBalance(u.recipient)));
const badTx = await contract.execute(badRecord, valid.bls, true, { gasLimit: GAS_LIMIT });
const badReceipt = await provider.waitForTransaction(badTx.hash, 1, 60_000);
assert(badReceipt && badReceipt.status === 0);
const afterBad = await Promise.all(members.map(u => contract.accounts(u.id)));
assert.deepEqual(afterBad.map(a => a.toArray()), beforeBad.map(a => a.toArray()));
assert.deepEqual(await Promise.all(members.map(u => provider.getBalance(u.recipient))), beforeBadBalances);
report.revertedTransaction = { hash: badTx.hash, blockNumber: badReceipt.blockNumber, status: badReceipt.status };
pass('mined invalid bundle reverts; no balance/nonce changes');
await contract.execute.staticCall(valid.records, valid.bls, true);
pass('valid bundle still accepted after every rejection');

// Demonstrate the attack that a naive same-message fast verifier admits.
const honest = users[0];
const attacker = bls.keygen(createHash('sha384').update('PUBLIC ROGUE-KEY DEMO').digest());
const rogue = attacker.publicKey.subtract(honest.publicKey);
rogue.assertValidity(); // Subgroup validation alone cannot stop a rogue key.
const commonHash = bls.hash(toUtf8Bytes('shared message'), DST);
const forged = bls.sign(commonHash, attacker.secretKey);
assert(bls.verify(forged, commonHash, bls.aggregatePublicKeys([honest.publicKey, rogue])));
pass('attack reproduced: naive same-message aggregate accepts without honest signature');
const fakePop = bls.sign(bls.hash(rogue.toBytes(true), POP), attacker.secretKey);
assert.equal(bls.verify(fakePop, bls.hash(rogue.toBytes(true), POP), rogue), false);
const honestPop = bls.sign(bls.hash(honest.publicKey.toBytes(true), POP), honest.secretKey);
assert(bls.verify(honestPop, bls.hash(honest.publicKey.toBytes(true), POP), honest.publicKey));
await rejected('rogue key with forged PoP rejected on-chain', () => contract.register.staticCall(g2(rogue), faucet.address, sigBytes(fakePop)), 'BLS proof of possession');
const beforeRegistration = await provider.getBalance(address);
const badRegistration = await contract.register(g2(rogue), faucet.address, sigBytes(fakePop),
  { value: 12345n, gasLimit: GAS_LIMIT });
const badRegistrationReceipt = await provider.waitForTransaction(badRegistration.hash, 1, 60_000);
assert(badRegistrationReceipt && badRegistrationReceipt.status === 0);
assert.equal(await contract.count(), 32n);
assert.equal(await provider.getBalance(address), beforeRegistration);
report.revertedRegistration = { hash: badRegistration.hash, blockNumber: badRegistrationReceipt.blockNumber,
  blockHash: badRegistrationReceipt.blockHash, status: 0 };
pass('mined rogue-key registration reverts; no account created or deposit retained');
// The standard PoP is reusable evidence about a key, not permission to migrate
// an existing identity. New account registration still accepts honest proofs.
await contract.register.staticCall(g2(honest.publicKey), honest.owner.address, sigBytes(honestPop));
pass('honest PoP still accepted after rogue-key rejection');

// Registration figures are reported separately, not silently charged to neither scheme.
const setupRaw = setupTxs.map(tx => Transaction.from(tx).serialized);
setupRaw.forEach((raw, i) => assert.equal(keccak256(raw), setupTxs[i].hash));
report.setup = { transactions: setupRaw.length, includesDeployment: true,
  gasUsed: setupReceipts.reduce((sum, r) => sum + BigInt(r.gasUsed), 0n).toString(),
  packedChannelEstimate: channel(setupRaw), registrationOnlyChannelEstimate: channel(setupRaw.slice(1)),
  receipts: setupReceipts };
const large = report.rounds.at(-1).normalizedChannelEstimate;
// 130,044 blob payload bytes minus a version byte and one 23-byte frame header.
// 14 is the BPO2 target, not the 21-blob burst maximum. Base competes for this budget.
const budget = 14 * (130_044 - 24) / 12;
const registration = report.setup.registrationOnlyChannelEstimate.compressed / users.length;
report.blobBudgetModel = { targetBlobsPerSlot: 14, secondsPerSlot: 12, payloadBytesPerSecond: budget,
  model: 'One full frame per blob, full utilization, entire L1 target available; scale TPS by Base share and fill ratio.',
  steadyState: { eoaBytesPerTransfer: large.directEoa.compressed / 32, blsBytesPerTransfer: large.bls.compressed / 32,
    eoaTps: budget * 32 / large.directEoa.compressed, blsTps: budget * 32 / large.bls.compressed },
  registrationBytesPerAccount: registration,
  amortized: [1, 2, 5, 10, 100].map(transfersPerKey => {
    const bytesPerTransfer = large.bls.compressed / 32 + registration / transfersPerKey;
    return { transfersPerKey, bytesPerTransfer, tps: budget / bytesPerTransfer,
      speedupVsEoa: (large.directEoa.compressed / 32) / bytesPerTransfer };
  }),
};
report.expectedAccounts = await Promise.all(users.map(async u => {
  const account = await contract.accounts(u.id);
  return { id: u.id, publicKey: account.publicKey, ecdsaOwner: account.ecdsaOwner,
    proof: u.proof, registrationHash: u.registrationHash,
    nonce: account.nonce.toString(), balance: account.balance.toString(),
    recipient: u.recipient, recipientBalance: (await provider.getBalance(u.recipient)).toString() };
}));
const checkpoint = await provider.send('eth_getBlockByNumber', [toBeHex(badRegistrationReceipt.blockNumber), false]);
report.checkpoint = { number: checkpoint.number, hash: checkpoint.hash, stateRoot: checkpoint.stateRoot,
  receiptsRoot: checkpoint.receiptsRoot, transactionsRoot: checkpoint.transactionsRoot };
report.scope = 'Real Base EVM PoP registration and aggregate-authorized transfers; not a native transaction fork or fee discount. Normalized synthetic SingleBatch/Brotli size estimates; L1-only derivation checked separately with verify-da.mjs. Research code, not audited.';
const json = JSON.stringify(report, null, 2);
if (process.argv[2]) writeFileSync(process.argv[2], `${json}\n`);
console.log(json);
provider.destroy();
