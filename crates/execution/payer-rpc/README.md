# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The upstream defaults to `--rollup.sequencer` when it is an HTTP URL; with a websocket sequencer, the proxy stays off unless `--rollup.payer` is set. `--rollup.payer` selects a different URL. Each upstream call times out after 15 seconds, longer than proxyd's 10-second payer timeout, so a hung payer cannot hold the node connection open. `payer_signTransaction` and `payer_getSponsorshipBalance` are not registered.

Requests are rejected locally before the Everest network upgrade activates EIP-8430 account abstraction transactions, when `chainId` does not match this node, or when the body is malformed. The node serves `payer_getTerms` from one shared entry, refetched on demand at most every 5 seconds, with concurrent misses sharing one upstream call. Serving rates that old relies on the payer honoring any rate it quoted within the offer `ttl`. The node does not poll; with no requests it makes no upstream calls. Each served offer's `ttl` is reduced by the entry's age, and its `tokens` are reordered by the request's `preferredTokens`. A cache miss is forwarded without `preferredTokens`, so the shared entry keeps the payer's own token order. A `TEMPORARILY_UNAVAILABLE` rejection from `payer_sendTransaction`, or a `PAYMENT_INSUFFICIENT` without a `requote`, refetches the entry early, at most once per 5 seconds. A rejection with a `requote` leaves the entry, since the wallet re-signs from the requote without calling `payer_getTerms`. When the payer cannot be reached, the last entry is served while its `ttl` holds; when the payer answers `payer_getTerms` with an error or no offers, the entry is dropped. `payer_sendTransaction` is never cached.

The cache is on by default when the payer is reached through `--rollup.sequencer`, whose payer returns the same terms for every request, and off for a separate `--rollup.payer`. `--rollup.payer-cache=true|false` overrides the default; leave it off for a payer that quotes per request.

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
