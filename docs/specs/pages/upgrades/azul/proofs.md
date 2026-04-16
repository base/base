# Azul: Proof System

Azul introduces a multi-proof system for the L2 checkpoints that secure withdrawals to L1. A
checkpoint is a fixed interval of L2 blocks summarized by an output root. Each proposal about that
checkpoint is submitted to `AggregateVerifier`, an L1 dispute game that can verify one or two
proofs for the same proposal before withdrawals rely on it.

In the common path, a TEE prover creates the initial proposal proof. A permissionless ZK prover can
later back the same proposal or dispute an invalid one. `AggregateVerifier` delegates proof checks
to dedicated verifier contracts, while a prover registrar keeps the onchain registry of accepted
TEE signer identities up to date.

## Why Change the Proof System

Base's current [fault-proof system](/protocol/fault-proof) is optimistic and interactive: a
proposal resolves unless someone challenges it. That model has two limits for Azul.

- Withdrawals take at least 7 days because every proposal inherits the full challenge window.
- Every bad proposal must be actively challenged. That creates an economic attack surface: if
  challengers cannot fund every dispute, an incorrect state can finalize. Centralized guardrails
  reduce that risk today, but that is not a long-term model for Stage 2 decentralization.

Azul replaces that model with a multi-proof design built around TEE and ZK provers. TEE proofs
support the common path, ZK proofs provide a permissionless backstop, and the architecture leaves
room to adopt stronger proving systems over time.

## Finality Model

The Azul design supports three settlement paths for a proposal on Ethereum:

| Proofs present | Settlement path | Target window | What it means                            |
| -------------- | --------------- | ------------- | ---------------------------------------- |
| TEE only       | Long window     | 7 days        | Common path, still overridable by ZK     |
| ZK only        | Long window     | 7 days        | Permissionless path without TEE reliance |
| TEE + ZK       | Short window    | 1 day         | Faster finality when both systems agree  |

The long window gives independent provers time to verify a claim and dispute it if needed. The
short window is available only when both proof systems back the same proposal. A ZK prover can also
dispute an invalid TEE-backed claim and claim the TEE prover's bond as a reward. In Azul, that delay
lives in `AggregateVerifier` itself. `OptimismPortal2` and `AnchorStateRegistry` no longer add a
separate 3.5 day delay, because keeping either legacy delay would eliminate the fast-finality path
even when both proofs are present.

## Security and Decentralization

- The TEE path is permissioned and optimized for the common case.
- The ZK path is permissionless and can override an invalid TEE-backed claim.
- The proof layer remains modular and can evolve toward stronger TEE implementations, different ZK
  systems, or multi-ZK designs.

## Overview

### New/Changed Onchain Components

- `AggregateVerifier`: Azul's dispute-game contract for checkpoint proposals. Each proposal is
  initialized with one proof, a second proof can be added later for the same claimed root, and the
  contract calls proof-specific verifier contracts and aggregates their results to determine how the
  proposal resolves. This is also where the Azul finality delay now lives.
- `TEEVerifier` and `ZKVerifier`: proof-specific verifier contracts called by `AggregateVerifier`.
  Their addresses are immutable on the `AggregateVerifier` implementation, so each deployment has
  an explicit verifier set.
- `DelayedWETH`: still escrows the proposal bond for each game, but Azul reduces its withdrawal delay
  to 1 day. That is sufficient here because the only bonds at stake are proposer bonds.
- `OptimismPortal2`: no longer adds the separate 3.5 day proof-maturity delay for these proposals.
  That timing moves into `AggregateVerifier`, which keeps the 1 day path reachable instead of
  forcing every proposal to inherit at least 3.5 days of extra delay.
- `AnchorStateRegistry`: Similar to `OptimismPortal2`, this no longer has a 3.5 day finalization
  delay for proposals, allowing fast finality.

### Proof Flow

The proof flow for Azul is:

1. The proposer identifies the next canonical checkpoint range and requests a TEE proof.
2. The TEE prover re-executes that L2 block range inside an AWS Nitro Enclave and signs the
   resulting output root.
3. The proposer verifies the result against canonical Base L2 state and submits a new
   `AggregateVerifier` game to L1.
4. A challenger can independently recompute the same checkpoint roots and, if it finds an invalid
   claim, sources the ZK proof needed to dispute it.

This architecture keeps the normal path simple, preserves a permissionless dispute path, and
supports faster settlement when both proof systems are available.

## Proof Roles

- The proposer turns canonical L2 checkpoints into new `AggregateVerifier` games on L1.
- A challenger checks in-progress games against canonical L2 state and disputes incorrect claims.
- TEE provers power the common proposal path.
- ZK provers provide the permissionless verification and override path.
- The registrar maintains the onchain registry of accepted TEE signer identities.
- `AggregateVerifier` and its verifier contracts verify claims before withdrawals on L1 can rely on
  them.

## Proposer

The proposer turns safe or finalized Base L2 checkpoints into L1 `AggregateVerifier` games. It
finds the latest canonical parent state, requests a TEE proof for the next checkpoint interval,
verifies the returned output root against canonical L2 state, and submits the next proposal with
the required bond.

## Challenger

Anyone can run a challenger. A challenger independently recomputes checkpoint output roots for
in-progress games, identifies the first invalid claim, and submits the required dispute
transaction. The permissionless dispute path is a ZK proof challenge. Base will run a challenger as
a security backstop, and Base's challenger also has access to a TEE nullification path for invalid
TEE-backed proposals.

## TEE Provers

TEE provers are AWS Nitro Enclave-backed services used in the common proposal path. The host gathers
witness data from RPCs, the enclave re-executes the requested L2 block range in isolation, and the
enclave signs the resulting checkpoint outputs with a key that never leaves the enclave.

## ZK Provers

ZK provers are the permissionless proving backend in Azul. They are used when a dispute requires a
ZK proof, especially to challenge an invalid TEE-backed proposal or to invalidate a bad ZK claim.
In normal operation, the proposer does not depend on ZK provers to create new games. In the
future, the proposer may integrate ZK provers directly so new roots can carry both proof paths from
the start, unlocking faster finality for all roots.

## Prover Registrar

The prover registrar keeps the onchain `TEEProverRegistry` in sync with the live set of Nitro prover
signers. It discovers active provers, attests their signer identities onchain, and removes orphaned
signers with safeguards against transient outages.
