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

### Separate validity transaction store

Every transaction submitted with validity predicates lives in a separate validity store inside
`BaseTransactionPool`, whatever its nonce shape: account (protocol) nonce, EIP-8130 2D-nonce
channel, or nonce-free. Normal transactions stay in the protocol pool and the EIP-8130 sidecar.
The validity store has its own capacity and its own EIP-8130 admission guard, so validity and
normal traffic never evict or rate-limit each other. The guards are separate ledgers: a payer's
combined reservations across both can exceed either ledger's allowance, and execution still
checks funds.

| Flag | Default |
| --- | --- |
| `--rollup.validity-pool-max-count` | the normal pending count cap (`--txpool.pending-max-count`) |
| `--rollup.validity-pool-max-size-mb` | the normal pending size cap (`--txpool.pending-max-size`) |
| `--rollup.validity-pool-max-account-slots` | the normal per-sender cap (`--txpool.max-account-slots`) |

Defaults are copied as independent allowances, so enabling validity traffic can roughly double
pool memory. Fee floors, locality, replacement price bumps and EIP-7702 delegated-account limits
follow the normal pool configuration. EIP-7702 limits count both classes: a transaction may not
authorize an address that holds more than the delegated slot limit across the protocol pool and
the validity store, and an authorization pooled in either one limits its authority's
account-nonce transactions in the other.

Account-nonce lanes in the validity store rank like the protocol pool: a transaction is pending
when every nonce from the canonical account nonce up to it is pooled, the lane's cumulative cost
through it fits the sender balance, and its fee cap covers the pending base fee. The block gas
limit is checked at insertion. Canonical nonce changes advance the lane even when this node never
stored the mined transaction. A reorg that rolls the account nonce back queues the lane until the
gap fills, where the protocol pool keeps its earlier pending flags. A transaction blocked only by
its fee cap is reported in the base fee subpool.

Validation reads canonical state, which can trail account updates the store already applied, such
as a flashblock that executed part of a lane. The store remembers each account's latest update for
64 updates, so an insertion never moves an account cursor backward or recreates a consumed nonce;
only an authoritative update (a reorg or account reload) moves it back. A remembered update's
balance backs the lane together with its nonce; an update that leaves the balance unchanged
keeps the validated one. A validation older than that window is rejected with
`PoolAdmissionError::StaleAccountState` (rejection label `stale_account_state`); resubmitting
revalidates it.

Within one sequential lane, `(sender, nonce_key)` with key zero meaning the account nonce, all
pooled transactions share one class. Adding a transaction of the other class fails with
`PoolAdmissionError::LaneConflict` (rejection label `lane_conflict`). The one exception is a
replacement of a lone incumbent at the same nonce that meets the replacement price bump: the
replacement is stored first and the incumbent is then removed and reported as discarded, so a
lookup can briefly see both. A nonce-free transaction's replay id is its slot: a same-replay-id
transaction of the other class must meet the replacement price bump and then replaces it the same
way.

Builders read one stream that merges the normal stores with the validity store by priority;
normal transactions win equal-priority ties. Invalidating a candidate only skips the rest of its
own store's lane.

The validity store reports `txpool.validity.pooled` (transactions) and
`txpool.validity.pooled_bytes`. Reth's `transaction_pool` gauges describe only the protocol pool,
and `txpool.guard.tracked` sums both guards.

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
