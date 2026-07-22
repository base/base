# base-execution-payer-rpc

ERC-8168 payer-service JSON-RPC for the Base builder-operated payer.

This crate exposes the `payer_*` namespace that wallets use to negotiate
token-denominated gas sponsorship for [EIP-8130] transactions, backed by the
pricing core in [`base-execution-payer`]:

- **`payer_getTerms`** — returns the currently-quotable terms (the co-signing
  payer account, whether the service is live, and each accepted token's
  `feeRecipient`, exchange [`Rate`](base_execution_payer::Rate) and payer
  margin). It is served directly from a per-block
  [`PriceSnapshot`](base_execution_payer::PriceSnapshot), so every Base node can
  answer it against head/pending state with no oracle round-trips.
- **`payer_sendTransaction`** — accepts a *partially-signed* EIP-8130
  transaction (the sender's `sender_auth` is present, `payer` designates the
  builder's payer EOA, and `payer_auth` is empty), co-signs it with the payer
  key, and submits the now fully-authorized transaction to the mempool.

## Why exclusive (no p2p), but reusing the mempool guards

A partially-signed transaction cannot ride p2p: the mempool only gossips
transactions that pass full validation, and "payer set but `payer_auth` empty"
is explicitly rejected by the txpool validator (the `payer`/`payer_auth` XOR).
So the builder co-signs **before** the transaction enters the pool, then inserts
it with [`TransactionOrigin::Private`](reth_transaction_pool::TransactionOrigin::Private)
— which runs the *identical* `BaseTransactionValidator` guards as any
externally-received EIP-8130
transaction (structural checks, sender/payer auth verification, nonce/replay,
intrinsic gas, payer balance) but sets `propagate = false`. The co-signed
transaction therefore never leaves this node, so the payer's ETH gas can only be
spent by this builder. If the phase-0 token transfer reverts at inclusion, the
builder simply discards the transaction as insufficient payment.

## Seams

The handler is generic over three collaborators so it unit-tests without a node:

- a [`TransactionPool`](reth_transaction_pool::TransactionPool) the co-signed
  transaction is inserted into,
- a [`PayerTerms`] resolver that produces the per-block
  [`PriceSnapshot`](base_execution_payer::PriceSnapshot) (the concrete
  state-backed reader lives with the node/builder wiring), and
- a [`PayerDigestSigner`](base_execution_payer::PayerDigestSigner) key backend
  (local key today, remote KMS/HSM later).

[EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
[ERC-8168]: https://ethereum-magicians.org/t/erc-8168-payer-services-for-erc-8130/28762
[`base-execution-payer`]: ../payer/README.md
