# `base-execution-payer`

ERC-8168 token payer for EIP-8130 transactions, run on the sequencer.

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
3. Reads the sender's balance through the token's `BalanceLayout` and rejects
   blacklisted or underfunded senders.
4. Signs `payer_signature_hash(sender)` with the payer account's own key, and
   sets `payer_auth` to `K1_AUTHENTICATOR || signature`.
5. Admits the co-signed transaction as a private validity transaction. Its
   predicates keep it includable only while the sender's balance still covers
   the payment, the sender is not blacklisted, and a `FiatToken` is not paused.
   A block-number bound at the ingress maximum is also required, and the pool
   evicts the transaction at `valid_before`.

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

`PayerConfig::resolve` verifies every feed and balance layout against an RPC
provider before the payer starts.

The payer account must qualify for the pool's high-rate payer limits, or the
pool caps its pending sponsored transactions. That means it must be delegated
to trusted proxy code and hard-locked.
