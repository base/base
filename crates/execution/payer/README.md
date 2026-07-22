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

Each accepted token in [`PayerConfig`] carries a [`PriceSource`] that is either
a **flat rate** or an external **feed**. A feed names the oracle contract, the
method [`selector`](FeedConfig::selector) that supplies the price, the
[`AnswerShape`] describing how to decode the return (e.g. Chainlink
`latestRoundData` vs. a bare single word), the answer [`FeedDirection`], and a
staleness bound. New oracle providers are onboarded by adding a decode shape
here and pointing the on-chain config at the new selector/shape — no contract
migration.

[ERC-8168]: https://ethereum-magicians.org/t/erc-8168-payer-services-for-erc-8130/28762
[EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
