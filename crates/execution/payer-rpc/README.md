# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The upstream defaults to `--rollup.sequencer`. `--rollup.payer` selects a different URL. `payer_signTransaction` and `payer_getSponsorshipBalance` are not registered.

Requests are rejected locally when EIP-8430 (Everest) is inactive, when `chainId` does not match this node, or when the body is malformed. `--rollup.payer-cache` serves `payer_getTerms` from one shared entry, refetched on demand at most once a second (about the payer's market poll, whatever the block time), with concurrent misses sharing one upstream call. The node does not poll; with no requests it makes no upstream calls. Each served offer's `ttl` is reduced by the entry's age, and its `tokens` are reordered by the request's `preferredTokens`. A `PAYMENT_INSUFFICIENT` or `TEMPORARILY_UNAVAILABLE` rejection from `payer_sendTransaction` refetches the entry early, at most once a second. When the payer cannot be reached, the last entry is served while its `ttl` holds; when the payer answers `payer_getTerms` with an error or no offers, the entry is dropped. Leave the flag off unless the payer returns the same terms for every request. `payer_sendTransaction` is never cached.

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
