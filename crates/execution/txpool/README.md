# `base-execution-txpool`

Transaction pool for Base.

## Overview

Extends Reth's transaction pool with Base-specific validation and ordering for the Base node.
`BaseTransactionValidator` enforces L1 data fee checks and Base-specific validity rules.
`BaseOrdering` and `TimestampOrdering` provide customizable transaction prioritization strategies.
Also includes a `BuilderApiImpl` for builder-specific pool management.

### Pluggable builder wire format

`ValidatedTransaction<E>` is the payload of `base_insertValidatedTransaction`, the endpoint mempool
nodes use to forward transactions to a builder. `E` carries additional wire fields and defaults to
`NoExtensions`, which encodes to exactly the same bytes as a struct without the field at all — so
the default is wire-compatible in both directions with peers that predate the parameter.

Downstream node builds substitute their own payload by implementing
`ValidatedTransactionExtensions<T>`, then registering the generic monomorphizations:

```rust,ignore
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, ExtensionError, ValidatedTransactionExtensions,
};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MyExtensions {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    my_field: Option<u64>,
}

impl ValidatedTransactionExtensions<MyPooledTx> for MyExtensions {
    fn extract(tx: &ValidPoolTransaction<MyPooledTx>) -> Self { /* ... */ }
    fn apply(self, tx: MyPooledTx) -> Result<MyPooledTx, ExtensionError> { /* ... */ }
}

// Builder (ingress) side, in place of the stock `BuilderApiExtension`:
let api = BuilderApiImpl::<_, MyExtensions>::with_extensions(pool);
modules.merge_configured(api.into_rpc())?;

// Mempool (egress) forwarding is provided by `base-tx-forwarding`.
```

Extension payloads must serialize as a JSON map (a braced struct, not a unit struct) and must avoid
`u128`/`i128` fields, which `serde_json` cannot represent through `#[serde(flatten)]`.

### Signed validity predicates

`TransactionValidity` carries off-chain predicates separately from the signed Ethereum
transaction. The transaction signature alone does **not** authorize those predicates.
Anyone holding the raw transaction can otherwise add a state predicate that delays
inclusion until their front-run has changed a pool's reserves. This can aid a sandwich
within the user's on-chain slippage limit; it does not change their signed calldata.

Signature enforcement is a default-off runtime feature. Enable
`--validity-require-signature` on forwarding ingress nodes and **every** builder.
On builders, the same flag covers both `base_sendRawTransactionValidity` and
`base_insertValidatedTransaction`. Each process must enable enforcement locally.
With enforcement on, non-empty predicates require `validity_signature` by the
transaction sender. With it off, unsigned behavior is unchanged and signed sidecars
are rejected, so clients cannot mistake acceptance for signature enforcement.
Mixed deployments do not provide this protection; unsigned builders still accept
unauthorized predicates. Query RPC nodes proxy the sidecar unchanged, leaving
enforcement to the upstream sequencer. Ordinary transactions need no extra signature.

#### Wallet signing contract

Sign the EIP-712 digest returned by `ValidityAuthorization::signing_hash` using
`eth_signTypedData_v4` (not `personal_sign`). Submit the signature as
`validity_signature` alongside `validity` in the RPC options or builder payload.
It uses Alloy's signature JSON object (`r`, `s`, and `yParity` hex quantities).
The domain is `{ name: "Base Transaction Validity", version: "1", chainId }`;
`chainId` is the transaction's chain ID. There is no verifying contract.

```text
ValidityPredicateData(uint8 kind,uint8 operator,address account,uint256 slot,uint256 mask,uint256 value)
ValidityAuthorizationData(bytes32 transactionHash,ValidityPredicateData[] validity)
```

`transactionHash` hashes the complete signed EIP-2718 transaction envelope.
`kind` is balance = 0, storage = 1, block number = 2, flashblock index = 3.
`operator` is `<` = 0, `<=` = 1, `=` = 2, `!=` = 3, `>` = 4, `>=` = 5.
Balance predicates use `account`, storage uses `account`, `slot`, and `mask`.
Unused fields are zero; omitted storage masks become `U256::MAX`.
All variants include `value`. Stable-sort predicates with
`ValidityPredicate::sort_batch` before signing: block number first, flashblock
index second, then state predicates in their submitted relative order.
The signature binds every predicate field, array length, canonical order, chain,
and transaction, preventing predicate mutation and cross-transaction replay.

Verification recovers the actual envelope sender rather than trusting the builder
payload's `sender`, and rejects high-s signatures. Only sender-address secp256k1
authorization is supported; contract wallets or EIP-8130 actors without that
key cannot use signed sidecars. Deposits and unprotected legacy transactions
have no chain-bound signing domain and are rejected when carrying predicates.
Shadow-only predicate injection is incompatible with signature enforcement.
Predicates remain builder-side metadata, not on-chain consensus rules: a holder
can still strip the entire sidecar and submit the underlying plain transaction.
This feature prevents unauthorized predicate attachment, not all sandwich attacks.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-txpool = { workspace = true }
```

```rust,ignore
use base_execution_txpool::{BaseOrdering, BaseTransactionPool, BaseTransactionValidator};

let pool = Pool::new(
    BaseTransactionValidator::new(client, evm),
    BaseOrdering::default(),
    config,
);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
