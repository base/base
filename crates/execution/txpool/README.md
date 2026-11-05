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

`transactionHash` hashes the complete signed EIP-2718 transaction envelope.
`kind` is balance = 0, storage = 1, block number = 2, flashblock index = 3, nonce = 4.
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
state flips or their block or flashblock expiry passes, so the validity pool has
its own count, size and per-sender caps and evicts only among validity
transactions. A burst of conditional transactions therefore cannot push standard
user transactions out of reth's pending subpool, and reth's own eviction cannot
drop resting validity transactions.

EIP-7702 delegation and validity predicates do not mix. The validator rejects a
transaction with validity predicates that is an EIP-7702 transaction or comes
from a delegated sender, and a state diff that changes an account's code drops
that account's validity transactions. The validity pool therefore never needs
reth's in-flight limits for delegated accounts and pending authorizations, which
apply only inside reth.

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
(`TwoDNoncePool::lanes_ahead_of_state`, `shared_lanes_pending_end`), so cross-store
bookkeeping visits those indexed lanes rather than scanning both stores. The validity
snapshot still scans its lanes and copies references to their pending transactions;
`benches/best_transactions.rs` measures iterator construction on a large pool.

Capacity defaults to reth's pending subpool limit (`--txpool.pending-max-count`,
`--txpool.pending-max-size`, `--txpool.max-account-slots`) and is overridable with
`--rollup.validity-pool-max-count`, `--rollup.validity-pool-max-size` (megabytes)
and `--rollup.validity-pool-max-account-slots`; see `ValidityPoolLimits`.
Occupancy is published as `txpool.validity.pool_transactions` and
`txpool.validity.pool_bytes`, evictions as `txpool.validity.evicted`, and
replacements that crossed a store boundary as
`txpool.validity.cross_store_replaced{direction}`.

#### Resting transactions

A validity transaction whose predicate failed in a build rests until a commit writes the state
it watches or a new block changes its condition; a wake asks for a recheck, not inclusion.
Expiry, removal and replacement drop a rest instead of waking it.

Pulling every resting transaction through the priority walk each build, only to park it again,
would make selection cost grow with the resting set. So when
`best_transactions_with_attributes_and_parking` gets a `BestTransactionsHold` that reports
resting transactions, it asks the hold about the validity pool's lane roots once, after
releasing the pool's locks and before ranking. A root (a lane's first pending transaction) is
asked only if it has validity predicates, is priced at the base fee, and is nonce-free or starts
at canonical state rather than behind another store's run (`lanes_ahead_of_state`). A held root
and the rest of its lane never enter the heap. Everything else, including roots left unasked
when the build is interrupted, takes the existing path: the parking adapter checks each
candidate it reaches.

```mermaid
stateDiagram-v2
    state "Checked by the iterator" as Checked
    state "Resting: out of the priority walk" as Resting
    [*] --> Resting: known rest at iterator creation
    [*] --> Checked: not held
    Checked --> Resting: predicate unsatisfied
    Checked --> Included: predicates satisfied, execution accepted
    Resting --> Checked: woken by a commit or a new block
    Resting --> Removed: expiry, removal or replacement
```

`promote` readies a root held at creation behind any claimant of its lane (a member already
yielded or parked, or the adapter's cached next candidate), with the rest of its run buffered
behind it and bridged standard successors released after it commits. `discard_parked`
invalidates its lane. Each iterator still snapshots the pool, so this saves the walk, not the
snapshot. Asking at creation marks held roots as seen for that block, so they stay retained
rather than aging out.

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
