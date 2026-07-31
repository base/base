# T4e egress red-line scope

**Status:** owner decision required; no policy or implementation option is selected.

## Decision question

The standing whole-build sentence is broader than the control that was designed. The fork says, “A build without `arm-live-egress` contains no `ProdBackend`, `reqwest`, socket opener, or live runtime variant” (`docs/s1b-runtime-switch-design.md:10-15`), while the detailed seal is specifically the transaction-submission path: `ProdBackend`, its optional direct `reqwest` dependency, and its socket calls compile only with `arm-live-egress` (`docs/s1b-runtime-switch-design.md:101-107`, `crates/execution/mev-trader-submit/Cargo.toml:52-63`, `crates/execution/mev-trader-submit/src/arm/transport.rs:616-637`). The owner must decide whether the red line means transaction submission or literally every byte sent by the node.

The CLI has no default features, but it unconditionally links `base-mev-trader` (`crates/execution/cli/Cargo.toml:41`, `crates/execution/cli/Cargo.toml:100-102`). The trader unconditionally exports Blink ingress and unconditionally carries Tokio WebSocket dependencies (`crates/execution/mev-trader/src/lib.rs:3-7`, `crates/execution/mev-trader/Cargo.toml:46-47`). Exact `MEV_TRADER_PHASE_A=1` plus Flashblocks configuration installs Phase A, and a credential file can construct and spawn the client (`crates/execution/cli/src/mev_trader.rs:8674-8683`, `crates/execution/cli/src/mev_trader.rs:8812-8826`, `crates/execution/cli/src/mev_trader.rs:9199-9206`, `crates/execution/cli/src/mev_trader.rs:9350-9364`).

Blink is receive-only at the application boundary, not zero-egress. Its credential is appended to the fixed connect URI, so it is carried in the TLS WebSocket request to the remote endpoint; after connection the client sends the fixed subscription and then reads the acknowledgment (`crates/execution/mev-trader/src/blink_ingress.rs:33-39`, `crates/execution/mev-trader/src/blink_ingress.rs:690-692`, `crates/execution/mev-trader/src/blink_ingress.rs:695-719`, `crates/execution/mev-trader/src/blink_ingress.rs:764-770`). Those are the two first-party production transmission sites. A third site is dependency-driven: tungstenite queues and writes an RFC 6455 Pong when its read loop receives Ping (`tungstenite-0.28.0/src/protocol/mod.rs:552-557`, `tungstenite-0.28.0/src/protocol/mod.rs:668-673`), and the Blink read loop drives that behavior (`crates/execution/mev-trader/src/blink_ingress.rs:853`). It emits bytes without another first-party `.send(` call.

The default-feature observation must not be converted into a claim that `reqwest` is absent from the node's resolved graph. `reqwest` is optional and live-gated only for `mev-trader-submit`; independently, the default node graph contains `reqwest@0.13.4 -> alloy-provider -> base-bundles -> base-metering -> base-execution-cli -> base-reth-node`. The existing arm-simulation tree seal also records a pre-existing `reqwest v0.12.28` baseline (`crates/execution/mev-trader-submit/tests/s2_capability_seal.rs:307-320`). Consequently, removing Blink from a default build would remove this known WebSocket exception, not establish whole-node `reqwest` absence or whole-node zero-egress.

## Existing protection and residual risk

The earlier capability-growth premise is narrowed, not withdrawn. The current trader seal fixes the exact dependency set and features, rejects signer/provider/transport dependency families, rejects signing, transaction insertion, forwarding, and raw-send identifiers, and scans an exact top-level production source-file inventory (`crates/execution/mev-trader/tests/capability_seal.rs:11-53`, `crates/execution/mev-trader/tests/capability_seal.rs:55-122`, `crates/execution/mev-trader/tests/capability_seal.rs:271-340`, `crates/execution/mev-trader/tests/capability_seal.rs:653-678`). For Blink specifically its literal-count pins detect proliferation or mutation of the existing endpoint, subscription, audited first-party `.send(` sink, connect call, message constructors, and socket/sink/credential accessors (`crates/execution/mev-trader/tests/capability_seal.rs:680-713`). They do not reject a differently shaped socket opener or write, even inside `blink_ingress.rs`, and do not establish an egress property for the crate's other twenty Rust files.

The residual review risks are: (i) source and repository-local seal remain editable together; (ii) Check B's bounded walk does not cover the trader or CLI today; and (iii) the trader's crate-wide seal is a signing/submission seal, not an egress seal—Tokio already enables `net`, and no trader test rejects a new socket primitive anywhere in the crate, including `blink_ingress.rs`. These are governance and walk-boundary risks, not evidence that Blink presently signs or submits transactions. The credential is the only sensitive value carried to the Blink socket; no ARM/T4e candidate, witness, signature, signed transaction, or signed artifact is passed to `BlinkFeedClient` (`crates/execution/mev-trader/src/blink_ingress.rs:652-678`, `crates/execution/cli/src/mev_trader.rs:8812-8819`).

A direct production-source scan finds zero `add_transaction`, `.pool()`, or `network()` uses in `crates/execution/mev-trader/src/`. The identifier seal forbids the `add_transaction` family (`crates/execution/mev-trader/tests/capability_seal.rs:93-95`, `:101`, `:110-114`, `:119`); `.pool()` and `network()` are blocked indirectly by forbidden `reth-transaction-pool` (`:74`) and `reth-network` (`:72`) dependency prefixes respectively, enforced at `:334-338`. No ARM/T4e signed artifact can reach this socket in the reviewed tree.

## Measured live state

The 2026-07-31 live-state inspection found PID `2344220`, started at `02:28:36`, running the current-tree binary built at `02:28:09`; it contains the Blink endpoint and does not predate the trader crate. Both `MEV_TRADER_PHASE_A` and `MEV_TRADER_BLINK_CREDENTIAL_FILE` were unset in the process environment and in `node.env`. Therefore the credential path was absent and `self.credential_file.and_then(...)` constructed no client (`crates/execution/cli/src/mev_trader.rs:8814-8822`). The process had zero connections to the three resolved Blink IPs while the inspection had a positive control. Revision `2d57e275b` contained no `base-mev-trader` crate; that historical contrast documents capability growth, not the deployed revision.

## Owner options

### (a) Rescope and record the red line

Define `arm-live-egress` as the compile-time gate for transaction/relay submission, and inventory receive-only Blink transport separately. Make the decision executable in both repositories:

- the canonical owner-decision sentence lives in base-mev `.omc/plans/DISPATCH-gjc-pr58-fixes-2026-07-30-addendum-b2.md:11`; amend that record rather than inventing a fork-local source for it;
- `git grep 'transmits bytes off the host' -- docs ':!docs/t4e-egress-redline-scope-design.md'` returns zero matches (rc 1, positive control 1), confirming that the canonical sentence is cross-repository rather than counting this brief's quotation;
- in this fork, change the literal `socket opener` wording at `docs/s1b-runtime-switch-design.md:12` to `transaction-submission socket opener`, and scope the `reqwest` statement to the submit crate's optional direct dependency.

This option is the smallest policy/documentation correction and preserves Phase A ingress. Its cost is accepting non-submission handshake/subscription bytes in an `arm-live-egress`-off node and maintaining the boundary explicitly. It does not claim that the whole default graph lacks `reqwest`.

### (b) Default-off feature-gate Blink ingress

Add a dedicated default-off trader feature around the Blink module/export and WebSocket dependencies, propagate it through the CLI, and gate client construction/spawn. The benefit is compile-time absence of this known Blink transport from artifacts that do not enable the feature. It does not remove unrelated node transports, prove all-default-node zero-egress, or eliminate `reqwest` from the default node graph.

The costs are concrete:

1. Phase A receive-only ingress disappears from the default build; the owner must decide whether the #176/#177 measurement lineage still requires that feed.
2. Using ingress requires deploying a non-default artifact, so the audited default artifact and executed artifact diverge from the red-line perspective.
3. The exact `SOURCE_FILES` and identifier/dependency seal must be reworked for both feature states (`crates/execution/mev-trader/tests/capability_seal.rs:37-53`, `crates/execution/mev-trader/tests/capability_seal.rs:653-678`).

The feature-rung cost is small, not a principal objection: `blink_ingress` is bare at `lib.rs:3`, while sibling trader modules already carry the `edge-measurement` cfg pattern at `lib.rs:9`, `:16`, and `:18` (`crates/execution/mev-trader/src/lib.rs:3-18`).

### (c) Extend the bounded Check B walk

Check B is `crates/execution/mev-trader-submit/tests/s2_capability_seal.rs`: `validate_no_production_egress` is at `:2965-2988`, its property sentence at `:2990`, the test and E-CTL/E-1..E-4 mutants at `:2991-3046`, and its walk root at `:2755-2759`. It walks only the submit crate today, so it proves nothing about either first-party Blink site.

Option (c) would extend that walk only across `crates/execution/mev-trader` and `crates/execution/cli/src/mev_trader.rs`, classifying the source-visible connect/handshake (`blink_ingress.rs:714-719`) and fixed subscription send (`:764-770`). It cannot see tungstenite's automatic Pong because that emitter is dependency code. Under a literal zero-egress policy these visible sites would make and keep Check B RED; under a submission-scoped policy they could be reviewed as constrained non-submission ingress. This option does not cure seal editability, classify dependency-emitted traffic, or establish anything about CLI/node code outside the stated walk.

## Composition and required decision

Options (a) and (c) compose: (a) makes the invariant truthful and records its boundary, while (c) makes the known non-submission exception and later changes in the bounded surface part of Check B. That combination is the likely minimum-cost path because it retains the current sealed receive-only feed, requires no alternate production artifact, and adds review visibility without pretending the entire node is walked. Option (b) remains the stronger compile-time isolation choice for Blink itself and carries the costs above.

Only the owner may choose (a), (b), (c), or the compatible (a)+(c) composition. This brief records no selection and authorizes no implementation, build, signing, activation, connection, deployment, or policy change.
