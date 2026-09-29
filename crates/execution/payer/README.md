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

```toml
[terms]
payer = "0x..."            # also the phase-0 recipient
max_expiry_secs = 10
quote_ttl_secs = 15
default_gas_limit = 100000 # used when payer_getTerms omits gasLimit
max_cost_wei = "0xB5E620F48000"

[eth_usd]
proxy = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70"
deviation_bps = 15

[[tokens]]
symbol = "USDC"
address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
decimals = 6
spread_bps = 100
payment_gas = 25000
probe_holder = "0x..."     # any holder with a balance, used to verify `balance`
balance = { layout = "fiat_token" }
price = { quote = "usd", legs = [
    { proxy = "0x7e860098F58bBFC8648a4311b374B1D669a2bc6B", deviation_bps = 30 },
] }
```

`TokenBook` resolves every token's feeds and verifies its balance layout
against the node's latest state, on first use and again once
`TokenBook::RESOLVE_INTERVAL_SECS` of chain time has passed. A token that fails
to resolve, for example because the node has not synced its feeds yet, is left
out of offers and rejected with `TEMPORARILY_UNAVAILABLE` until it resolves.

## Running

A block-building node (`base sequencer` or `base-builder`) serves the payer
when started with `--payer.config <path>` and the payer account's key, from
either `--payer.key` or a hex file at `--payer.key.path`. The key must control
`terms.payer`. The node adds the payer to `--rollup.mempool-trusted-payers`,
so the pool bounds its pending sponsored transactions only by its ETH balance.

Nodes started with `--rollup.sequencer` forward `payer_*` to the sequencer,
passing its rejections through unchanged.
