# `base-execution-txpool`

Transaction pool for Base.

## Overview

Extends Reth's transaction pool with Base-specific validation and ordering for the Base node.
`BaseTransactionValidator` enforces L1 data fee checks and Base-specific validity rules.
`BaseOrdering` and `TimestampOrdering` provide customizable transaction prioritization strategies.
Also includes a `BuilderApiImpl` for builder-specific pool management.

### EIP-8130 transaction identity

The EIP-8130 transaction hash covers the body and the resolved sender, not `sender_auth` or
`payer_auth`. Different valid encodings can therefore share one hash, for example two open
payers, or two valid `sender_auth` blobs for the same sender that cost different gas. The pool
keys transactions by hash, so the first variant it sees wins: a later variant with the same
hash is rejected as already known (`AlreadyImported`, not a bad transaction), and it cannot
replace the first one, because its fees are the same. Once the first variant leaves the pool
(mined, evicted, or dropped), another variant is admitted like a new transaction; the pool
keeps no fallback copy. Rejections that depend only on auth bytes return
`Eip8130Authentication`, which is also not a bad transaction, so a variant that fails them
does not blacklist the hash.

### Pluggable builder wire format

`ValidatedTransaction<E>` is the payload of `base_insertValidatedTransaction`, the endpoint mempool
nodes use to forward transactions to a builder. `E` carries additional wire fields and defaults to
`NoExtensions`, which encodes to exactly the same bytes as a struct without the field at all — so
the default is wire-compatible in both directions with peers that predate the parameter.

Downstream node builds substitute their own payload by implementing
`ValidatedTransactionExtensions<T>`, then registering the generic monomorphizations:

```rust,ignore
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, DEFAULT_MAX_VALIDITY_PREDICATES, ExtensionError,
    ValidatedTransactionExtensions,
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
let api = BuilderApiImpl::<_, MyExtensions>::with_extensions(pool, true, DEFAULT_MAX_VALIDITY_PREDICATES);
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

Use the shared `--validity-signature-mode off|verify-if-present|required` option
on forwarding ingress nodes and builders. It defaults to `off`: unsigned and signed
predicates are accepted without signature verification. Supplied signatures, even invalid
ones, are preserved for forwarding; malformed wire encodings and invalid predicate
parameters are still rejected. On builders the mode covers both
`base_sendRawTransactionValidity` and `base_insertValidatedTransaction`.

In `off` mode, nodes accept signed clients during rollout, but do not authenticate them.
Deploy `verify-if-present` to **every** ingress and builder so raw ingress verifies supplied signatures
while continuing to accept unsigned predicates. Migrate wallets and load clients to
signing, observe signed/unsigned admission counters, then switch the fleet to `required`.
At raw ingress, `required` rejects any non-empty predicate batch without valid sender
authorization. Builder insertion checks signature presence only and trusts the forwarder
to have verified it. Plain transactions need no extra signature.
Query nodes proxy sidecars unchanged and leave policy to the upstream sequencer.

**Off and optional verification modes are compatibility stages, not sandwich protection.**
As long as unsigned predicates are accepted, an intermediary can remove a
signature and submit different unsigned predicates. Do not claim protection
until **every forwarding ingress node** and each builder's raw ingress runs `required`,
each builder's insert policy requires signatures, and `base_insertValidatedTransaction`
is reachable only by trusted forwarders. A `required` builder alone is not protection:
an `off` forwarder can pass through a dummy signature and modified predicates, which
the builder's presence-only check will accept. Mixed-mode deployments are rollout
stages, not an authorization guarantee.

#### Wallet signing contract

Sign the EIP-712 digest returned by `ValidityAuthorization::signing_hash` using
`eth_signTypedData_v4` (not `personal_sign`). Submit the signature as
`validity_signature` alongside `validity` in the RPC options or builder payload.
It uses Alloy's signature JSON object (`r`, `s`, and `yParity` hex quantities).
The domain is `{ name: "Base Transaction Validity", version: "1", chainId }`;
`chainId` is the transaction's chain ID. There is no verifying contract.

```text
ValidityPredicate(uint8 kind,uint8 operator,address account,uint256 slot,uint256 mask,uint256 value)
ValidityAuthorization(bytes32 transactionHash,ValidityPredicate[] validity)
```

`transactionHash` is the transaction hash returned by `eth_sendRawTransaction`.
For every type except EIP-8130 (`0x79`) it is the hash of the complete signed
EIP-2718 envelope. The EIP-8130 transaction hash covers the body and the resolved
sender but not `sender_auth` or `payer_auth`, so it is not `keccak256` of the
encoding. One signature authorizes every same-hash variant, for example
different open payers, and a signature over `keccak256` of the encoding is rejected.
`kind` is balance = 0, storage = 1, block number = 2, flashblock index = 3, nonce = 4.
Flashblock-index predicates still decode but are rejected at ingress: Base no longer
builds flashblocks.
`operator` is `<` = 0, `<=` = 1, `=` = 2, `!=` = 3, `>` = 4, `>=` = 5.
Balance and nonce predicates use `account`; storage uses `account`, `slot`, and `mask`.
Nonce predicates read the account's protocol nonce, not an EIP-8130 channel nonce.
Unused fields are zero; omitted storage masks become `U256::MAX`.
`ValidityPredicateKind` and `ValidityOperator` define these explicit wire IDs.
All fields participate in each predicate's EIP-712 struct hash. Sort the typed
predicates by their struct hashes in ascending byte order before signing;
`ValidityAuthorization::canonical_predicates` builds this wallet-facing array.
Retain duplicates. Conjunction permutations produce the same digest; adding,
removing, or changing a predicate changes it. Evaluation uses its independent
cheap-first ordering and does not affect the signing contract.

Raw ingress reuses the recovered envelope sender to verify sidecar signatures
and rejects high-s signatures. Only sender-address secp256k1
authorization is supported; contract wallets or EIP-8130 actors without that
key cannot use signed sidecars. Deposits and unprotected legacy transactions
have no chain-bound signing domain and cannot authorize signed predicates.
Shadow-only predicate injection is incompatible with signature enforcement.
Predicates remain builder-side metadata, not on-chain consensus rules: a holder
can still strip the entire sidecar and submit the underlying plain transaction.
This feature prevents unauthorized predicate attachment, not all sandwich attacks.

`ValidityAuthorization::validate_recovered` reuses the sender already recovered at raw
ingress, including the builder's own `base_sendRawTransactionValidity` endpoint.
`base_insertValidatedTransaction` trusts forwarding mempool nodes to verify signatures
and only checks the local mode against signature presence. It does not recover either
the envelope sender or the sidecar signer again; restrict this endpoint to trusted forwarders.
Generic extension attachment does not take a validity-specific mode or enforce signature policy.
`with_validity(TransactionValidity)` attaches the sidecar without signature verification.
Raw ingress calls `validate_recovered` before attachment; the trusted builder insert
path attaches directly, then checks the local signature-presence policy.

`txpool.validity_signature.rejected{site,reason}` records bounded rejection reasons
at `ingress` and `builder`, independently of generic extension errors.
`txpool.validity_signature.accepted{site,signature}` separates signed and unsigned
sidecars to help operators assess migration readiness. RPC signature errors carry
the same stable rejection reason in their `data` field.

### Validity pool

`BaseTransactionPool` keeps three stores behind one `TransactionPool` surface:
the reth `Pool` for standard transactions, the EIP-8130 sidecar for finite
2D-nonce channels and nonce-free transactions, and a validity pool for every
transaction that carries validity predicates (conditional transactions), whatever
its envelope type. Validity transactions rest in the pool until a predicate on hot
state flips or their block deadline passes, so the validity pool has
its own count, size and per-sender caps and evicts only among validity
transactions. A burst of conditional transactions therefore cannot push standard
user transactions out of reth's pending subpool, and reth's own eviction cannot
drop resting validity transactions.

Delegation and validity predicates do not mix. The validator rejects a
transaction with validity predicates that is an EIP-7702 transaction, carries an
EIP-8130 delegation entry, or comes from a delegated sender, and a state diff that
changes an account's code drops that account's validity transactions. The
validity pool therefore never needs reth's in-flight limits for delegated
accounts and pending authorizations, which apply only inside reth. An EIP-8130
delegation always uses the protocol nonce, so it lands in reth, which counts its
sender as a pending authority, as for an EIP-7702 authorization.

`PoolStore::of` picks the store. A nonce slot `(sender, nonce key, nonce)`, or a
nonce-free replay id, is held by at most one store at a time: a newcomer bound for
another store replaces the incumbent under the usual price-bump rule and the
incumbent's listeners see `Replaced`. Account semantics span the stores: a
validity transaction right behind reth's or the sidecar's pending run is pending
(`LaneCursor::next_nonce` follows that run), while a standard transaction behind a
resting validity transaction stays queued in reth until the nonce gap closes.
`best_transactions` merges the three stores by priority but never yields a
sender's nonce `n + 1` before `n`: a validity head continuing a standard run waits
until the merge has yielded the transaction before it, and standard transactions
queued right behind a validity run are released by the merge once that run has
been yielded, so a predicate-delayed transaction does not keep its descendants out
of the same block. The lanes that straddle stores are indexed incrementally
(`TwoDNoncePool::lanes_ahead_of_state`, `shared_lanes_pending_end`), so building
the iterator costs those lanes plus each store's own snapshot, not the pool sizes;
`benches/best_transactions.rs` measures it on a large pool.

Capacity defaults to reth's pending subpool limit (`--txpool.pending-max-count`,
`--txpool.pending-max-size`, `--txpool.max-account-slots`) and is overridable with
`--rollup.validity-pool-max-count`, `--rollup.validity-pool-max-size` (megabytes)
and `--rollup.validity-pool-max-account-slots`; see `ValidityPoolLimits`.
Occupancy is published as `txpool.validity.pool_transactions` and
`txpool.validity.pool_bytes`, evictions as `txpool.validity.evicted`, and
replacements that crossed a store boundary as
`txpool.validity.cross_store_replaced{direction}`.

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
