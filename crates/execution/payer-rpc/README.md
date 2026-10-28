# `base-execution-payer-rpc`

RPC proxy for the network payer service (ERC-8168).

Query nodes that opt in expose `payer_getTerms` and `payer_sendTransaction` on the execution RPC and forward them to the payer. The methods are off by default, so a node relays wallet payer traffic only when its operator chooses to. `--rollup.payer-enabled` forwards them to the sequencer URL when it is an HTTP URL: `--rollup.sequencer`, or `--rpc.forwarding-endpoint` on `base rpc`. `--rollup.payer` selects a different URL and enables the methods on its own, including on a node whose sequencer URL is a websocket; sequencer headers are not sent to it. Each upstream call times out after 15 seconds, so a hung payer cannot hold the node connection open. `payer_signTransaction` is not registered.

Requests are rejected locally before the Everest network upgrade activates EIP-8430 account abstraction transactions, when `chainId` does not match this node, when `payer_getTerms` names no sender in `sender` or its alias `from`, or names different ones in both, when the body is malformed, or when the transaction is larger than 128 kibibytes, the transaction pool's default limit.

`payer_sendTransaction` also checks the transaction before forwarding it, and rejects it with the `PAYER_REJECTED` error a payer would return. It must be an EIP-8130 transaction for this chain that names a `payer`, leaves `payer_auth` empty for the payer to fill, and carries a sender signature that recovers; otherwise the rejection is `INVALID_TRANSACTION`. Before the Zenith keystore upgrade, a declared sender must also sign with its own secp256k1 (K1) key. A nonce that its channel already used at the latest block is rejected the same way, which stops a recently included transaction from being resubmitted with its `payer_auth` stripped; nonce-free transactions rely on their expiry instead, and a nonce that cannot be read forwards the transaction. The node does not execute the transaction: the payer simulates it before co-signing and answers a failed payment more precisely than the node could, for example `SENDER_BALANCE_INSUFFICIENT` with the shortfall and the offers the sender can switch to.

`payer_sendTransaction` also checks the transaction before forwarding it, and rejects it with the `PAYER_REJECTED` error a payer would return. It must be an EIP-8130 transaction for this chain that names a `payer`, leaves `payer_auth` empty for the payer to fill, and carries a sender signature that recovers; otherwise the rejection is `INVALID_TRANSACTION`. Before the Zenith keystore upgrade, a declared sender must also sign with its own secp256k1 (K1) key. A nonce that its channel already used at the latest block is rejected the same way, which stops a recently included transaction from being resubmitted with its `payer_auth` stripped; nonce-free transactions rely on their expiry instead, and a nonce that cannot be read forwards the transaction. The node does not execute the transaction: the payer simulates it before co-signing and answers a failed payment more precisely than the node could, for example `SENDER_BALANCE_INSUFFICIENT` with the shortfall and the offers the sender can switch to.

Both methods answer `TEMPORARILY_UNAVAILABLE`, which wallets retry, when the node cannot serve them right now: the payer does not answer, or the node cannot read its latest block. A payer answers with `PAYER_REJECTED` (`-32000`) or a standard request error (`-32601`, `-32602`), and those pass through unchanged whatever the HTTP status of the reply. Any other error, or no response, means the payer did not answer: the upstream cannot be reached, or a gateway in front of the payer answered in its place. A payer may have submitted a transaction whose answer was lost; a retry resends the same signed transaction, so it cannot be included twice.

The node serves `payer_getTerms` from one shared entry, refetched on demand at most every 5 seconds, with concurrent misses sharing one upstream call. Serving rates that old relies on the payer honoring any rate it quoted within the offer `ttl`. The node does not poll; with no requests it makes no upstream calls. Each served offer's `ttl` is reduced by the entry's age, and its `tokens` are reordered by the request's `preferredTokens`. A cache miss is forwarded without `preferredTokens`, `kinds` or `fiatCurrency`, so the shared entry keeps every offer kind in the payer's own token order and currency; wallets filter offers by kind themselves, as they must for a payer that ignores the hint. Only token offers are shared, without the `gasEstimate` and `paymentAmount` sized for the intent that fetched them: a response with sponsorship terms or declines, which depend on the sender, goes to its caller and is not stored. A `TEMPORARILY_UNAVAILABLE` rejection from `payer_sendTransaction`, or a `PAYMENT_INSUFFICIENT` without a `requote`, refetches the entry early, at most once per 5 seconds. A rejection with a `requote` leaves the entry, since the wallet re-signs from the requote without calling `payer_getTerms`. When the payer does not answer, the last entry is served while its `ttl` holds, after which requests answer `TEMPORARILY_UNAVAILABLE`, and requests that queued behind that refetch reuse its outcome instead of each waiting out the timeout; when the payer answers `payer_getTerms` with `TEMPORARILY_UNAVAILABLE` or no offers, the entry is dropped, while an error about one request, such as invalid params, leaves it. `payer_sendTransaction` is never cached.

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
