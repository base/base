# base-execution-payer

Pricing primitives for the [ERC-8168] payer service that Base nodes serve for
[EIP-8130] account-abstraction transactions.

This crate is the **pure, chain-read-free core** of the payer feature: given an
on-chain payer configuration and (for feed-backed tokens) a decoded oracle
reading, it produces the ERC-8168 `rate` and the exact phase-0 `paymentAmount`
a wallet must transfer to have its gas paid in a token. It performs no EVM
calls, holds no keys, and serves no RPC — those live in the layers above:

- **Reader layer** (builder / node RPC) supplies a [`FeedReading`] by
  `STATICCALL`-ing the configured oracle (or reading a storage slot) against
  head/pending state, then calls into this crate.
- **RPC layer** turns the resulting [`Rate`] / `paymentAmount` into
  `payer_getTerms` offers.
- **Builder** re-derives the amount at co-sign time to validate the phase-0
  transfer before filling `payer_auth`.

## Model

Each accepted token in [`PayerConfig`] carries a [`PriceSource`], one of:

- **Flat** — a fixed [`Rate`], no external read.
- **Slot** ([`SlotFeed`]) — the price is `SLOAD`ed straight from a known storage
  slot on the aggregator and a [`SlotField`] bit-field extracts the answer
  (with an optional [`SlotTimestamp`] slot for staleness). This is the **fast,
  deterministic builder path**: one cold `SLOAD` per token against the pending
  build state, cacheable per block, with no EVM execution — pricing stays
  consensus-consistent with the block being built, and a sender's balance can be
  checked the same way (a single `SLOAD` of the token's balance slot).
- **Feed** ([`FeedConfig`]) — the price is ABI-decoded from an oracle
  `STATICCALL` return, using the method [`selector`](FeedConfig::selector) and an
  [`AnswerShape`] (e.g. Chainlink `latestRoundData` vs. a bare word).

All feed/slot sources share a [`FeedDirection`] and a staleness bound. New
oracle providers are onboarded by adding a decode shape / slot layout here and
pointing the on-chain config at it — no contract migration. If the payer's
phase-0 token transfer reverts at inclusion (insufficient payment), the builder
simply discards the transaction.

## `storage` feature

The optional `storage` feature adds `PayerConfigStorage`, the native
read/write mirror of the on-chain payer-config system contract. It decodes the
contract's storage — an enumerable accepted-token set plus per-token terms
packed into a single word — into the pure [`PayerConfig`] model, and exposes the
admin mutations that back the round-trip tests. This is the concrete reader the
builder and node RPC use against head/pending state; enabling it pulls in the
native `base-precompile-storage` machinery (and, transitively, `revm`), so the
default build stays the pure pricing core.

## `signer` feature

The optional `signer` feature adds the payer co-signer. The builder-operated
payer is a full-owner secp256k1 EOA (scope `0x00`); `PayerCosigner` authorizes a
sponsored EIP-8130 transaction just-in-time by signing its payer digest and
wrapping the signature as the canonical k1 `payer_auth` blob
(`K1_AUTHENTICATOR || r || s || v`). The key backend is the
`PayerDigestSigner` trait — `LocalPayerSigner` holds a local key today, and a
remote (KMS/HSM) backend can implement the same trait. This is a distinct key
from the pricing core and pulls in `base-common-consensus` and `k256`.

[ERC-8168]: https://ethereum-magicians.org/t/erc-8168-payer-services-for-erc-8130/28762
[EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
