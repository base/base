# base-execution-eip8130-state

A native, read-only mirror of the EIP-8130 `AccountConfiguration` system
contract's storage layout.

This crate is the **state layer** of the EIP-8130 validation pipeline. Where
[`base-execution-eip8130`] performs the stateless *Authenticate* step (verify a
signature, resolve an `actorId`), this crate reads the on-chain account
configuration that the stateful *Authorize* step needs: an actor's authenticator
binding, scope, expiry and policy gate, plus per-account sequences and lock
status.

## Why read storage natively

EIP-8130 validation runs on both the mempool-admission and block-inclusion
paths, and must be cheap and EVM-free in the pool. Rather than `STATICCALL` into
`AccountConfiguration`, the protocol reads its storage directly — exactly as the
`NonceManager` / `TxContext` precompiles do. This crate models the contract's
storage with the same [`base-precompile-storage`] `#[contract]` + `Mapping`
abstraction, so the reader runs over any `PrecompileStorageProvider`:

- `HashMapStorageProvider` for unit tests,
- the live EVM journal at block inclusion, and
- (later) a `StateProvider` adapter for mempool admission.

## Storage layout

Mirrors the deployed contract (plain sequential slots, no ERC-7201 namespace):

```solidity
mapping(bytes32 actorId => mapping(address account => ActorConfig)) _actorConfig;     // slot 0
mapping(bytes32 actorId => mapping(address account => bytes32))     _policyCommitment; // slot 1
mapping(bytes32 actorId => mapping(address account => address))     _policyManager;    // slot 2
mapping(address account => AccountState)                            _accountState;     // slot 3
```

`account` is the inner mapping key (the contract's ERC-7562 storage-access
requirement). `ActorConfig` (`address,uint8,uint48,uint8`) and `AccountState`
(`uint64,uint64,uint40,uint16`) each pack into one slot; because the storage
abstraction has no `uint48`/`uint40` primitive, the packed slot is read as a raw
`U256` and unpacked manually to preserve exact Solidity packing.

## ⚠️ Provisional

The `AccountConfiguration` address and storage layout track the in-flux
`base/eip-8130` reference contracts; both the address (via
[`base_common_consensus::Eip8130Contracts`]) and this layout must be re-pinned if
the contract changes. A differential test against the deployed contract is a
planned follow-up.

[`base-execution-eip8130`]: ../eip8130
[`base-precompile-storage`]: ../../common/precompile-storage
