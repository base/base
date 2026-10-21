# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The upstream defaults to the sequencer URL when it is an HTTP URL: `--rollup.sequencer`, or `--rpc.forwarding-endpoint` on `base rpc`. With a websocket sequencer, the proxy stays off unless `--rollup.payer` is set. `--rollup.payer` selects a different URL. Each upstream call times out after 15 seconds, longer than proxyd's 10-second payer timeout, so a hung payer cannot hold the node connection open. `payer_signTransaction` and `payer_getSponsorshipBalance` are not registered.

Requests are rejected locally before the Everest network upgrade activates EIP-8430 account abstraction transactions, when `chainId` does not match this node, or when the body is malformed.

`payer_sendTransaction` also checks the transaction before forwarding it, and rejects it with the `PAYER_REJECTED` error a payer would return. It must be an EIP-8130 transaction for this chain that names a `payer`, leaves `payer_auth` empty for the payer to fill, and carries a sender signature that recovers; otherwise the rejection is `INVALID_TRANSACTION`. Before the Zenith keystore upgrade, a declared sender must also sign with its own secp256k1 (K1) key. A nonce that its channel already used at the latest block is rejected the same way, which stops a recently included transaction from being resubmitted with its `payer_auth` stripped; nonce-free transactions rely on their expiry instead. The node then runs the first call phase, which carries any token payment, at the latest block through its EIP-8130 `eth_call`. A revert is rejected as `EXECUTION_REVERTED` with `revert.phase` 0, and a halt or an invalid transaction as `INVALID_TRANSACTION`. Later phases are not run, since they can fail without voiding the payment. A nonce read or simulation that cannot run, such as one without state, forwards the transaction, which the payer checks again.

The node serves `payer_getTerms` from one shared entry, refetched on demand at most every 5 seconds, with concurrent misses sharing one upstream call. Serving rates that old relies on the payer honoring any rate it quoted within the offer `ttl`. The node does not poll; with no requests it makes no upstream calls. Each served offer's `ttl` is reduced by the entry's age, and its `tokens` are reordered by the request's `preferredTokens`. A cache miss is forwarded without `preferredTokens`, so the shared entry keeps the payer's own token order. A `TEMPORARILY_UNAVAILABLE` rejection from `payer_sendTransaction`, or a `PAYMENT_INSUFFICIENT` without a `requote`, refetches the entry early, at most once per 5 seconds. A rejection with a `requote` leaves the entry, since the wallet re-signs from the requote without calling `payer_getTerms`. When the payer cannot be reached, including when proxyd answers in its place because the payer backend is offline, unhealthy, over capacity, or slow, or the node is over a rate limit, the last entry is served while its `ttl` holds, and requests that queued behind that refetch reuse its outcome instead of each waiting out the timeout; when the payer answers `payer_getTerms` with an error or no offers, the entry is dropped. `payer_sendTransaction` is never cached.

The cache is on by default when the payer is reached through the sequencer URL, whose payer returns the same terms for every request, and off for a separate `--rollup.payer`. `--rollup.payer-cache=true|false` overrides the default; leave it off for a payer that quotes per request.

## Usage

```rust,ignore
use base_execution_payer_rpc::{PayerRpcConfig, PayerRpcExtension};

runner.install_ext::<PayerRpcExtension>(PayerRpcConfig {
    upstream_url: sequencer_url,
    upstream_headers: sequencer_headers,
    cache_get_terms: false,
});
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
