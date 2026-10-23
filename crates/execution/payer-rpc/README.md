# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes that opt in expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The methods are off by default, so a node relays wallet payer traffic only when its operator chooses to. `--rollup.payer-enabled` forwards them to the sequencer URL when it is an HTTP URL: `--rollup.sequencer`, or `--rpc.forwarding-endpoint` on `base rpc`. `--rollup.payer` selects a different URL and enables the methods on its own, including on a node whose sequencer URL is a websocket; sequencer headers are not sent to it. Each upstream call times out after 15 seconds, so a hung payer cannot hold the node connection open. `payer_signTransaction` and `payer_getSponsorshipBalance` are not registered.

Requests are rejected locally before the Everest network upgrade activates EIP-8430 account abstraction transactions, when `chainId` does not match this node, when the body is malformed, or when the transaction is larger than 128 kibibytes, the transaction pool's default limit.

`payer_sendTransaction` also checks the transaction before forwarding it, and rejects it with the `PAYER_REJECTED` error a payer would return. It must be an EIP-8130 transaction for this chain that names a `payer`, leaves `payer_auth` empty for the payer to fill, and carries a sender signature that recovers; otherwise the rejection is `INVALID_TRANSACTION`. Before the Zenith keystore upgrade, a declared sender must also sign with its own secp256k1 (K1) key. A nonce that its channel already used at the latest block is rejected the same way, which stops a recently included transaction from being resubmitted with its `payer_auth` stripped; nonce-free transactions rely on their expiry instead. A transaction queued behind an earlier nonce of its sender is forwarded without simulating its first call phase, which may depend on that earlier transaction. The node then runs the first call phase, which carries any token payment, at the latest block through its EIP-8130 `eth_call`. It runs at the simulation's gas cap rather than the transaction's gas limit, since the simulation charges worst-case intrinsic gas; whether that limit is enough is left to the payer and the pool. A revert is rejected as `EXECUTION_REVERTED` with `revert.phase` 0, and a halt or a transaction the EVM refuses, such as one whose sender cannot pay, as `INVALID_TRANSACTION`. Later phases are not run, since they can fail without voiding the payment. Any other failure forwards the transaction, which the payer checks again: a simulation that cannot run, such as one without state or one that times out, a nonce that cannot be read, which forwards without a simulation since the transaction may be queued, and the validity-window check, which runs on the latest block's clock and so trails the pool's.

Both methods answer `TEMPORARILY_UNAVAILABLE`, which wallets retry, when the node cannot serve them right now: the payer does not answer, or the node cannot read its latest block. A payer answers with `PAYER_REJECTED` (`-32000`) or a standard request error (`-32601`, `-32602`), and those pass through unchanged whatever the HTTP status of the reply. Any other error, or no response, means the payer did not answer: the upstream cannot be reached, or a gateway in front of the payer answered in its place. A payer may have submitted a transaction whose answer was lost; a retry resends the same signed transaction, so it cannot be included twice.

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
