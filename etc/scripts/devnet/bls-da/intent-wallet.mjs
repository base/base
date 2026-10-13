// Development-only wallet API. Keys must be backed up by the caller; this example
// does not provide production key storage or concurrent nonce management.
import assert from 'node:assert/strict';
import { toQuantity } from 'ethers';
import { CHAIN_ID, TYPE, REGISTRY, registration, signNative, signTransfer } from './native-codec.mjs';

export class NativeBlsWallet {
  #provider;
  #owner;
  #key;
  #fees;

  constructor(provider, owner, key, fees = {
    maxPriorityFeePerGas: 1_000_000n, maxFeePerGas: 100_000_000_000n,
  }) {
    this.#provider = provider;
    this.#owner = owner;
    this.#key = key;
    this.#fees = fees;
  }

  // One-time ECDSA-authorized creation publishes the public key and its PoP on DA.
  // Wait for this hash's receipt before transferring. Existing EOA funds stay put.
  async create() {
    assert.equal((await this.#provider.getNetwork()).chainId, CHAIN_ID);
    const nonce = await this.#provider.getTransactionCount(this.#owner.address, 'pending');
    const { input } = registration(this.#key);
    const gasLimit = BigInt(await this.#provider.send('eth_estimateGas', [{
      type: TYPE, chainId: toQuantity(CHAIN_ID), from: this.#owner.address, to: REGISTRY,
      value: '0x0', nonce: toQuantity(nonce), input, gas: toQuantity(1_000_000),
      ...Object.fromEntries(Object.entries(this.#fees).map(([k, v]) => [k, toQuantity(v)])),
    }]));
    return this.#provider.send('eth_sendRawTransaction', [
      signNative(this.#owner, input, { nonce, gasLimit, ...this.#fees }),
    ]);
  }

  // Each user signs and submits one transfer. No batch, aggregator or relayer is
  // exposed to the wallet; the sender pays its own fixed 21,000 gas plus DA fees.
  async transfer({ to, value }) {
    assert.equal((await this.#provider.getNetwork()).chainId, CHAIN_ID);
    const nonce = await this.#provider.getTransactionCount(this.#owner.address, 'pending');
    const tx = signTransfer({ sender: this.#owner.address, nonce, recipient: to, value,
      ...this.#fees }, this.#key.secretKey);
    const hash = await this.#provider.send('eth_sendRawTransaction', [tx.raw]);
    assert.equal(hash, tx.hash, 'RPC must identify the canonical intent, not its signature');
    return hash;
  }
}
