# `base-execution-payer`

ERC-8168 token payer for EIP-8130 transactions, run on the block-building node.

The payer accepts ERC-20 tokens for gas. A wallet builds a transaction that
names the payer, pays it in phase 0 with a single `IERC20.transfer`, and
submits it through `payer_sendTransaction` with `payer_auth` left empty. The
payer then:

1. Checks that `valid_before` is set and at most `max_expiry_secs` away, and
   that gas is within `max_gas_limit` and `max_cost_wei`.
2. Prices the token from Chainlink aggregator storage at the latest state (see
   `base-common-price-feed`), adds the token's spread, and requires the phase-0
   amount to cover `gas_limit × max_fee_per_gas` at that rate. A shortfall is
   rejected with `PAYMENT_INSUFFICIENT` and a `requote`.
3. Reads the sender's balance through the token's `BalanceLayout`, rejecting
   an underfunded sender with `SENDER_BALANCE_INSUFFICIENT`, then simulates the
   transfer from the sender at the latest state. A transfer that reverts, halts,
   or returns `false` (a blacklisted sender or paused token, for example) is
   rejected with `EXECUTION_REVERTED`.
4. Signs `payer_signature_hash(sender)` with the payer account's own key, and
   sets `payer_auth` to `K1_AUTHENTICATOR || signature`.
5. Admits the co-signed transaction as a private validity transaction. Its
   predicates keep it includable only while the sender's balance still covers
   the payment, up to a block-number bound at the ingress maximum, and the pool
   evicts it at `valid_before`.

The price is checked once at co-sign; it is not a predicate. The spread must
cover the summed deviation thresholds of a token's feeds, because a feed may
lag the market by that much without a new round.

`payer_getTerms` quotes one token offer covering every token whose price is
readable. Only `payer_sendTransaction` is offered, since the payer relays
everything it co-signs.

## Configuration

The payer reads one TOML file, passed with `--payer.config`. A Base mainnet
config accepting USDC:

```toml
[terms]
payer = "0x1111111111111111111111111111111111111111" # replace with the payer account
max_expiry_secs = 30            # longest valid_before the payer co-signs, from now
quote_ttl_secs = 15             # how long wallets may cache a quote
default_gas_limit = 100000      # intent gas assumed when payer_getTerms omits gasLimit
max_gas_limit = 1000000         # optional ceiling on gas_limit
max_cost_wei = "0xB5E620F48000" # optional ceiling on gas_limit × max_fee_per_gas

[eth_usd]                       # converts USD-quoted token prices to ETH
proxy = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70"
deviation_bps = 15

[[tokens]]
symbol = "USDC"
address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
decimals = 6
spread_bps = 100
payment_gas = 40000
probe_holder = "0x2222222222222222222222222222222222222222" # replace with a USDC holder
balance = { layout = "fiat_token" }
price = { quote = "usd", legs = [
    { proxy = "0x7e860098F58bBFC8648a4311b374B1D669a2bc6B", deviation_bps = 30 },
] }
```

Unknown keys are rejected, so typos fail at startup rather than being
ignored.

### `[terms]`

- `payer`: the account that pays gas and receives the phase-0 transfer. The
  node's payer key must control it.
- `max_expiry_secs`: the payer rejects a transaction whose `valid_before` is
  unset, already passed, or further away than this. Keep it at or below the
  node's validity-transaction lifetime (60 seconds by default), since the
  co-signed transaction carries a block-number bound at that limit.
- `quote_ttl_secs`: advertised as the offer's `ttl`. The payer re-prices every
  submission, so this only tells wallets when to fetch new terms.
- `default_gas_limit`: gas for the intent's own calls when `payer_getTerms`
  omits `gasLimit`. The quoted gas limit adds the largest `payment_gas` on top.
- `max_gas_limit`, `max_cost_wei`: optional per-transaction ceilings. Omit
  them to accept any size. `max_cost_wei` is a hex or decimal string.

### `[eth_usd]` and feeds

Every feed entry takes a Chainlink **proxy** address, not an aggregator. The
payer follows the proxy to its current aggregator and reads the latest round
from aggregator storage, so it keeps working across Chainlink phase upgrades
and does not depend on an RPC provider. Find proxies and deviation thresholds
on the feed's page at [data.chain.link](https://data.chain.link), selecting
the Base network.

- `proxy`: the feed proxy.
- `deviation_bps`: the feed's deviation threshold in basis points (0.5% is
  `50`). A feed may lag the market by this much without publishing a round.

Heartbeats are not enforced: a stable price legitimately goes without a new
round, and the deviation threshold still bounds how far the stored answer can
be from the market.

`[eth_usd]` is required even when every token is quoted in ETH.

### `[[tokens]]`

- `symbol`, `decimals`: shown to wallets. `decimals` must match the token
  contract, since rates are in the token's atomic units.
- `address`: the token contract, or its proxy for upgradeable tokens.
- `spread_bps`: markup over the feed-implied rate. It must be at least the
  summed `deviation_bps` of every feed on the token's price path, including
  `[eth_usd]` for USD-quoted tokens, or the config is rejected. Add margin on
  top for swap costs and volatility between co-signing and inclusion.
- `payment_gas`: gas the phase-0 transfer adds. Measure it with
  `eth_estimateGas` for a `transfer` to the payer, and keep headroom: a
  transfer that creates the payer's balance slot costs more than later ones.
- `probe_holder`: any account with a non-zero balance of the token, such as a
  treasury or large holder. At startup, and every
  `TokenBook::RESOLVE_INTERVAL_SECS` of chain time after, the payer checks
  that the balance it reads through `balance` equals `balanceOf(probe_holder)`.
  Pick an account that is unlikely to empty.
- `balance`: where the token stores balances, so predicates can gate
  inclusion on the sender's balance without executing the token.
  - `{ layout = "fiat_token" }`: Circle `FiatTokenV2_2`, used by USDC, EURC,
    and cbBTC.
  - `{ layout = "mapping", slot = "0x3" }`: a plain
    `mapping(address => uint256)` at that slot. For Solidity tokens, find the
    slot with `forge inspect <Contract> storageLayout` or the verified
    source's storage layout; `OpenZeppelin` `ERC20` uses slot `0`, solmate
    `ERC20` uses slot `3`, and upgradeable `OpenZeppelin` v5 tokens use
    namespaced storage, which `mapping` does not cover.
- `price`: the feeds whose product is the token's price.
  - `quote = "usd"`: the legs price the token in USD, and `[eth_usd]`
    converts to ETH.
  - `quote = "eth"`: the legs price the token in ETH directly. At least one
    leg is required.
  - Each leg is `{ proxy, deviation_bps, invert }`. Leave `invert` false
    (the default) when the feed reports the price of the token, or of the
    previous leg's currency, in the next currency. Set it when the feed is
    quoted the other way round, such as a `USD / XYZ` feed for a token pegged
    to XYZ.

Typical price paths:

| Token | `quote` | Legs |
| --- | --- | --- |
| USDC, other USD stablecoins | `usd` | the token's `/ USD` feed |
| EURC | `usd` | `EURC / USD` or `EUR / USD` |
| cbBTC | `usd` | `cbBTC / USD` or `BTC / USD` |
| cbETH | `eth` | `cbETH / ETH` |
| Local-currency stablecoin | `usd` | `XYZ / USD`, or `USD / XYZ` with `invert = true` |

`TokenBook` resolves every token's feeds and verifies its balance layout
against the node's latest state, on first use and again once
`TokenBook::RESOLVE_INTERVAL_SECS` of chain time has passed. A token that fails
to resolve, for example because the node has not synced its feeds yet, is left
out of offers and rejected with `TEMPORARILY_UNAVAILABLE` until it resolves,
and the node logs a warning naming the token and the reason.

### Startup checks

The node refuses to start when:

- `max_expiry_secs` is zero;
- a token address is listed twice;
- an ETH-quoted token has no legs;
- a token's `spread_bps` is below the summed deviation of its feeds; or
- the payer key does not control `terms.payer`.

Feed and balance-layout checks need chain state, so they run when the payer
first serves a request rather than at startup.

## Running

A block-building node (`base sequencer` or `base-builder`) serves the payer
when started with `--payer.config <path>` and the payer account's key, from
either `--payer.key` or a hex file at `--payer.key.path`. The same settings
are read from `BASE_PAYER_CONFIG`, `BASE_PAYER_KEY`, and
`BASE_PAYER_KEY_PATH`. Prefer the key file, readable only by the node's
user, over putting the key on the command line. The node adds the payer to
`--rollup.mempool-trusted-payers`, so the pool bounds its pending sponsored
transactions only by its ETH balance.

Keep the payer account funded with ETH. Swapping collected tokens back to ETH
is not implemented yet, so the balance only goes down.

Nodes started with `--rollup.sequencer` forward `payer_*` to the sequencer,
passing its rejections through unchanged.

To try the payer on a local devnet with mock feeds and a mock USDC, see
"Token payer demo" in `etc/docker/README.md`.
