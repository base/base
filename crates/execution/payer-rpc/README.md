# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The upstream defaults to `--rollup.sequencer`. `--rollup.payer` selects a different URL. `payer_signTransaction` and `payer_getSponsorshipBalance` are not registered.

Requests are rejected locally when EIP-8430 (Everest) is inactive, when `chainId` does not match this node, or when the body is malformed. `--rollup.payer-cache` caches successful `payer_getTerms` responses as one shared entry for the offer `ttl` (capped at five minutes). Leave it off unless the payer returns the same token catalog for every request. `payer_sendTransaction` is never cached.

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
