# base-common-evm2 — EVM2 integration roadmap

This document tracks the full path to Base/OP-Stack execution parity on the
[`evm2`](https://github.com/danipopes/evm2) engine, replacing the revm-based
`base-common-evm` path. It records what has already landed (in trunk and across
the open PR stack), and what this spike drives to completion.

`evm2` ships **zero** OP/Base code — it is a generic, associated-type-driven EVM
framework (`EvmTypesHost`, `TxRegistry`, `TxHandlerHooks`, `#[instruction]`
opcodes, `system_call` + system-contract address constants,
`BlockStateAccumulator`, `Precompiles::base(spec)`). Everything Base-specific is
built here, with the revm crate `base-common-evm` as the behavioral parity bar.

## Status legend

- ✅ done — landed in trunk or on the current PR stack
- 🚧 in progress — implemented in this spike
- ⬜ todo — remaining

## Foundation (landed on trunk / PR stack)

| Area | Status | Where |
| --- | --- | --- |
| `BaseEvmTypes` type family (`EvmTypesHost`) | ✅ | trunk (#4693 scaffold) |
| Engine-neutral L1 fees extracted | ✅ | trunk (#4700) |
| `BaseTxEnvelope` (deposit + standard) | ✅ | trunk (#4695) |
| Tx registry: deposit `0x7e` + legacy/2930/1559/7702 | ✅ | trunk (#4696) |
| Standard-tx handler hooks wired | ✅ | trunk (#4696) |
| `BaseSpecId` fork schedule (BaseUpgrade → evm2 SpecId) | ✅ | #4761 |
| Deposit execution semantics + differential parity harness | ✅ | #4759 |
| L1 data fee + Isthmus operator fee + 3-vault distribution | ✅ | #4762 |
| Block-executor core (tx loop, receipts, cumulative gas) | ✅ | #4765 |
| Pre-execution hooks: EIP-2935 block-hashes + EIP-4788 beacon-roots | ✅ | #4767 |

Type 3 (EIP-4844 blob) is intentionally unregistered — Base rejects blob txs.

## Remaining work (this spike drives to 100%)

### Phase 1 — Block-execution completeness

| Item | Status | Notes |
| --- | --- | --- |
| Block gas-limit pre-check (`reserved > available`) | ✅ | pre-Regolith deposits exempt |
| Executor chain-spec injection (`BaseForkActivations`) | ✅ | revm-free fork schedule (genesis `UpgradeConfig`) |
| Irregular-state flush into `BlockStateAccumulator` | ✅ | `IrregularStateChange` (commit_source + visit) |

### Phase 2 — Transition-block system hooks

| Item | Status | Notes |
| --- | --- | --- |
| Canyon create2-deployer force-deploy | ✅ | one-shot on first Canyon block |
| Denim BaseTime predeploy install | ✅ | EIP-1967 proxy linkage + admin validation |
| Cobalt EIP-8130 system-account stub | ✅ | `0xEF` reap-protection stub |

### Phase 3 — Jovian / Azul metering

| Item | Status | Notes |
| --- | --- | --- |
| Jovian DA-footprint per-tx metering + block limit | ✅ | FastLZ estimate vs DA scalar (in `L1FeeParams`) |
| `blob_gas_used` surfacing in `BlockExecutionResult` | ✅ | carries the accumulated DA-footprint gas |
| Azul EIP-7825 per-tx gas cap (16,777,216) | ✅ | enforced by evm2 handlers at Osaka; deposits exempt |

### Phase 4 — Block result completeness

| Item | Status | Notes |
| --- | --- | --- |
| Post-block balance increments (empty withdrawals for OP) | ✅ | no-op for OP: no block reward, no in-body withdrawals |
| `blob_gas_used` result field | ✅ | carries the accumulated DA-footprint gas (Phase 3) |
| State-gas accounting in cumulative gas | ✅ | `tx_gas_used` matches revm in the block parity harness |
| Full block-level differential parity test | ✅ | drives evm2 executor vs revm sequentially |
| EIP-7685 `requests` result field | ⬜ | OP emits none today; add if/when a request type lands |

### Phase 5 — Precompiles

| Item | Status | Notes |
| --- | --- | --- |
| `BaseEvmTypes::precompiles()` + Fjord P256VERIFY | ✅ | RIP-7212 at Fjord (ahead of upstream Osaka) |
| Base bn254 pairing caps (Granite/Jovian) + BLS caps (Isthmus/Jovian) | ✅ | input-capped variants delegating to evm2's precompiles |
| Dynamic installs (B20, registries, TxContext, NonceManager) | ⬜ | Beryl/Cobalt custom contracts (revm-typed reference) |
| Precompile metrics observer + storage-feature gating | ⬜ | |

### Phase 6 — EIP-8130 enshrined account abstraction (type `0x79`)

> **Scope note.** The reference EIP-8130 engine (`base-common-evm`'s `eip8130.rs`, ~3,400 lines,
> plus the revm-based `base-execution-eip8130` crate) is deeply coupled to revm's `Evm` internals.
> Porting it onto evm2 without pulling revm into this crate is a large, self-contained track and is
> **not** started in this spike; the items below are the decomposition. Only the receipt arm
> (`0x79 → BaseReceiptEnvelope::Eip8130`) and the block-gas payer-auth reservation shape exist so
> far.

| Item | Status | Notes |
| --- | --- | --- |
| Envelope variant (`BaseTxEnvelope::Eip8130`) | ✅ | carries `Eip8130Signed`; type byte + gas limit |
| Registry handler + verified/simulate execution | ⬜ | needs the EIP-8130 execution engine |
| 2D nonce-manager **storage layout** (slot derivation + read) | ✅ | `NonceManager`; slot parity vs revm reference |
| 2D nonce increment + events + replay ring + validity window | ⬜ | needs the EIP-8130 execution context |
| Authorizer / policy gate, sender/payer split | ⬜ | |
| Phased calls, custom intrinsic gas, fee settlement | ⬜ | |
| Simulate gas-limit bisection | ⬜ | |

### Phase 7 — Node integration

> **Scope note.** Wiring the crate into the node depends on the upstream `alloy-evm`/`reth` EVM2
> bridge, which does not exist yet (the scaffold, #4693, deliberately keeps this crate un-wired
> "before the upstream alloy-evm and reth EVM2 bridge lands"). This phase is **blocked on
> upstream** and cannot land here until that bridge is available.

| Item | Status | Notes |
| --- | --- | --- |
| alloy-evm / reth EVM2 bridge | ⬜ | blocked on upstream bridge |
| End-to-end block import parity | ⬜ | blocked on the above |

## Spike status summary

Landed and tested in this spike (revm-free non-test build preserved throughout):

- **Phase 1 (complete)** — block gas-limit pre-check; executor fork-schedule input
  (`BaseForkActivations`) and irregular-state flush (`IrregularStateChange`).
- **Phase 2 (complete)** — Canyon create2-deployer, Denim `BaseTime`, and Cobalt EIP-8130
  system-account transition hooks, with a **differential parity** harness proving byte-identical
  installed state against the revm reference functions.
- **Phase 3 (complete)** — Jovian DA-footprint metering + `blob_gas_used`; Azul EIP-7825 per-tx
  gas cap (already enforced by the evm2 handlers, pinned by tests).
- **Phase 4 (complete)** — block-level differential parity harness (cumulative gas + per-tx
  success vs revm across Ecotone/Fjord/Isthmus); `blob_gas_used` and state-gas accounting; OP
  post-block balance increments are a no-op (no block reward, no in-body withdrawals).
- **Phase 5 (partial)** — `BaseEvmTypes::precompiles()` with Fjord `P256VERIFY` and the bn254/BLS
  input caps (cap constants pinned to the revm reference).
- **Phase 6 (started)** — EIP-8130 transaction envelope variant (`BaseTxEnvelope::Eip8130`) and the
  `NonceManager` storage layout (2D channel nonces + nonce-free replay protection), slot-parity
  tested against the revm reference.

Remaining, in rough size order, with the reason each is out of scope for this spike:

- **Phase 5** — Beryl/Cobalt **dynamic** precompiles (B20 factory, registries, `TxContext`,
  `NonceManager`) only. The static bn254/BLS input caps and Fjord P256 are done (evm2-native
  variants delegating to evm2's precompiles). The dynamic ones are custom stateful contracts whose
  reference lives in the revm-typed `base-common-precompiles`, so they need an evm2-native
  reimplementation with their own differential validation.
- **Phase 6 — EIP-8130 execution engine (XL).** The envelope variant and nonce storage layout are
  done; the remainder (registry handler, intrinsic-gas compute, dispatch/authorizer/policy gate,
  phased-call execution, fee settlement, verified/simulate) is ~3,000 lines tightly coupled to the
  revm-typed `base-execution-eip8130` subsystem and evm2 execution semantics — its own multi-PR
  track, only end-to-end validatable as a whole.
- **Phase 7 — node wiring.** Blocked on the upstream `alloy-evm`/`reth` EVM2 bridge, which does not
  exist yet.
