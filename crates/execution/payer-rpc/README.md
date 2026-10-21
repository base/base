# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes that opt in expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The methods are off by default, so a node relays wallet payer traffic only when its operator chooses to. `--rollup.payer-enabled` forwards them to the sequencer URL when it is an HTTP URL: `--rollup.sequencer`, or `--rpc.forwarding-endpoint` on `base rpc`. `--rollup.payer` selects a different URL and enables the methods on its own, including on a node whose sequencer URL is a websocket; sequencer headers are not sent to it. Each upstream call times out after 15 seconds, so a hung payer cannot hold the node connection open. `payer_signTransaction` and `payer_getSponsorshipBalance` are not registered.

Requests are rejected locally before the Everest network upgrade activates EIP-8430 account abstraction transactions, when `chainId` does not match this node, when the body is malformed, or when the transaction is larger than 128 kibibytes, the transaction pool's default limit.

Both methods answer `TEMPORARILY_UNAVAILABLE`, which wallets retry, when the node cannot serve them right now: the payer does not answer, or the node cannot read its latest block. A payer answers with `PAYER_REJECTED` (`-32000`) or a standard request error (`-32601`, `-32602`), and those pass through unchanged. Any other error, or no response, means the payer did not answer: the upstream cannot be reached, or a gateway in front of the payer answered in its place. A payer may have submitted a transaction whose answer was lost; a retry resends the same signed transaction, so it cannot be included twice.

## Usage

```rust,ignore
use base_execution_payer_rpc::{PayerRpcConfig, PayerRpcExtension};

runner.install_ext::<PayerRpcExtension>(PayerRpcConfig {
    upstream_url: sequencer_url,
    upstream_headers: sequencer_headers,
});
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
