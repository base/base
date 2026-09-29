# base-common-price-feed

Onchain Chainlink price feed reads and [ERC-8168] token rates.

A sequencer-operated payer prices gas in tokens from Chainlink feeds on the
chain itself, so quoting at co-sign and re-checking at inclusion read the same
state and need no offchain price service.

[`ChainlinkFeed::read`] reads a feed's latest answer with two storage reads
through a [`revm::Database`], which works both against a provider's latest
state and against the builder's in-progress block. It needs no EVM call:

1. The `s_hotVars` word holds `latestAggregatorRoundId` at bit offset 48.
2. `s_transmissions[roundId]` packs `int192 answer`, `uint32 startedAt`, and
   `uint32 updatedAt` from low to high bits in one slot.

Slot indices differ by aggregator implementation. [`ChainlinkLayout`] covers:

| `typeAndVersion()` | `s_hotVars` slot | `s_transmissions` slot |
|---|---|---|
| `AccessControlledOCR2Aggregator 1.0.0` | 11 | 12 |
| `DualAggregator 1.0.0` (SVR feeds) | 13 | 17 |

Wallets and config name a feed by its proxy, which delegates to an aggregator
that can change on a Chainlink phase upgrade. [`ChainlinkFeed::resolve`] follows
the proxy to its aggregator, selects the layout from `typeAndVersion()`, and
rejects the feed unless a storage read equals `latestRoundData()` at the same
block. Callers resolve at startup and re-resolve to pick up phase upgrades.

[`PricePath`] multiplies one or more feeds, each optionally inverted (for
feeds quoted the other way round, such as `USD / ARS`), into a token price in
USD or ETH, and converts it to a [`TokenRate`]: token atomic units per 10^18
wei, the ERC-8168 `rate`. [`TokenRate::required_amount`] computes the phase-0
payment `ceil(gas_limit * max_fee_per_gas * rate / 10^18)` exactly as the ERC
specifies.

[ERC-8168]: https://github.com/ethereum/ERCs/pull/1555
