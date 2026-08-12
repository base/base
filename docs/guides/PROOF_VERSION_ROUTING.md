# N-version proof routing

Status: Phase 1 implementation  
Research date: 2026-08-08

## Decision summary

Keep one prover-service queue per network and run N version-pinned worker fleets against it. Reuse
the exact-match job filter from closed base/base PR
[#4265](https://github.com/base/base/pull/4265), but treat its integer `protocol_version` as an
opaque routing ID resolved from a per-game capability descriptor, not as a hardcoded binary
revision or a legacy/current boolean.

The descriptor should contain the commitments that determine whether a worker can produce a proof
accepted by a game:

```text
journal schema / schedule mode
CONFIG_HASH
TEE_IMAGE_HASH
ZK_RANGE_HASH
ZK_AGGREGATE_HASH
```

For the old full-schedule era it must also identify the pinned `scheduleId` and the rollup-config
snapshot needed to reproduce it. The challenger can read the hashes directly from the historical
game proxy. A small, reviewed registry maps the canonical descriptor to a `u32` routing ID. New
verifiers should expose an immutable `proofProtocolId()` so this mapping stops relying on getter
probing.

This supports N versions. A job at routing ID 7 is claimable only by a worker announcing 7; adding
IDs 8 and 9 does not alter or drain ID 7.

Do not extend `NitroEnclavePool` for cross-version routing initially. Keep each host process
homogeneous and use the existing pool only for multiple copies of the same enclave image. This is
the smallest safe operational boundary because the current pool neither tags enclaves by PCR0 nor
matches `ProofRequest.image_hash` when selecting one.

## Repositories and current state

| Repository | Observed head | Relevant state |
| --- | --- | --- |
| `base/base` | `84ef08b` on 2026-08-08 | PR #4209 and PR #4144 are merged |
| `base/contracts` | `db08d3a` | Current `AggregateVerifier` has activated-prefix schedule pinning and public commitment getters |
| `protocols/base-proofs` | `cc8c854` on `origin/master` | Production build, Terraform, Helm, Sif, Odin, and pipeline definitions live here |

The `protocols/base-proofs` findings use its `origin/master` at the research date.

### PR findings

- [base/base #4209](https://github.com/base/base/pull/4209) merged on 2026-08-07. The PR's final
  head was `13ad2566` and merge commit was `e1a8cf1d`; `f4460348` was an earlier rebased head used
  as #4265's parent, not the final merge commit. The full diff adds
  `schedule_l2_block_number`, derives the activated schedule prefix, and uses the game's ending L2
  block for challenger subranges.
- [base/base #4265](https://github.com/base/base/pull/4265) was read in full at `4d3f47de`.
  Its DB column and exact-match claim predicates are N-capable, but every new worker announces the
  same compile-time constant, and the challenger distinguishes only `scheduleId()` success/revert.
  It therefore routes only two assumed formats and does not bind TEE or ZK artifacts.
- GitHub contains no stated closure rationale for #4265. It received only two bot review comments,
  its base ref was force-pushed on 2026-08-06 at 16:31 UTC, and the author closed it six minutes
  later with no linked issue or follow-up comment. The Aug 3 design concerns are consistent with
  closing it, but that motivation is not recorded publicly and should not be presented as fact.
- The bot concern about `scheduleId() == bytes32(0)` is valid, but checking for nonzero would not
  fix it. Zero is a valid activated-prefix result when no upgrade is active, and is also the seed
  in earlier schedule code. Classify getter/schema capability, never the returned value's truthiness.
- [base/base #4144](https://github.com/base/base/pull/4144) merged on 2026-08-08 as `84ef08b`.
  Nitro now uses `crates/proof/worker/src/job_discovery.rs`; all claim-version plumbing must be
  implemented there, not in the deleted Nitro-local loop.

### Concurrent changes to coordinate

Open base/base PRs with direct overlap as of the research date:

| PR | Overlap |
| --- | --- |
| [#4319](https://github.com/base/base/pull/4319) | `challenge`, `contracts`; standalone dispute-game proving workflow |
| [#4257](https://github.com/base/base/pull/4257) | Large pre-dynamic-upgrade backport across challenge, prover-service, and Nitro; explicitly excludes `schedule_id` |
| [#4281](https://github.com/base/base/pull/4281) | Draft predecessor of #4319 touching challenge/contracts |
| [#4030](https://github.com/base/base/pull/4030) | Draft cancellation RPC touching prover protocol, DB, service, and challenge mocks |
| [#4047](https://github.com/base/base/pull/4047) | Dirty proposer cleanup PR touching AggregateVerifier bindings |
| [#3587](https://github.com/base/base/pull/3587) | Draft Nitro proof-generator logging |

The production repo also has overlapping drafts:

- [base-proofs #407](https://coinbase.ghe.com/protocols/base-proofs/pull/407) pins proposer,
  prover-service, and Nitro host to #4257's no-schedule backport. It is high-risk, dirty, and
  explicitly requires those components to stay on the same journal format.
- [base-proofs #406](https://coinbase.ghe.com/protocols/base-proofs/pull/406) and
  [#387](https://coinbase.ghe.com/protocols/base-proofs/pull/387) are draft Nitro host pins for
  the now-merged shared-worker refactor.
- [base-proofs #158](https://coinbase.ghe.com/protocols/base-proofs/pull/158) is a stale draft that
  adds `VSOCK_CIDS` for multiple enclaves. It assumes old/new images may share one host, which is
  not sufficient routing by itself.

## Commitment inventory

The onchain verifier commits different fields for TEE and ZK proofs:

```text
common = proposer || l1OriginHash || startingRoot || startingBlock
         || endingRoot || endingBlock || intermediateRoots || CONFIG_HASH

TEE journal = keccak256(common || TEE_IMAGE_HASH || [scheduleId])
ZK journal  = keccak256(common || ZK_RANGE_HASH || [scheduleId])
ZK verifier additionally verifies the proof against ZK_AGGREGATE_HASH.
```

`[scheduleId]` is absent only in the oldest era.

| Commitment | Produced / configured by | Consumed by | Current challenger visibility |
| --- | --- | --- | --- |
| Journal layout | Rust `ProofJournal` and SP1 aggregation program; matching Solidity `AggregateVerifier` | Nitro signer, SP1 programs, AggregateVerifier | Not identified per game |
| `CONFIG_HASH` | Immutable AggregateVerifier deploy arg; Nitro uses `PerChainConfig`, SP1 hashes it | Both journals | Public getter exists; Rust client does not bind it |
| Schedule mode/value | `ProtocolVersions`; game pins a value; Rust `BootInfo::load` derives it | Both journals and proof execution config | Era/value are not read by the Rust client |
| `TEE_IMAGE_HASH` | AggregateVerifier deploy arg; actual image hash is `keccak256(PCR0)` in the enclave | TEE journal and TEEVerifier image check | Public getter exists; challenger leaves request `image_hash` at zero |
| `ZK_RANGE_HASH` | SP1 range ELF verification key, embedded into zk-host at build time | ZK journal and aggregation program | Public getter exists; Rust client does not bind it |
| `ZK_AGGREGATE_HASH` | SP1 aggregation ELF verification key, embedded into zk-host at build time | ZKVerifier / SP1 verifier | Public getter exists; Rust client does not bind it |
| TEE/ZK verifier behavior | `TEE_VERIFIER` / `ZK_VERIFIER` immutable addresses | AggregateVerifier | Public getters exist; Rust client does not bind them |

### End-to-end ownership

1. The challenger scanner reads game state and intervals. It currently resolves intervals through
   the factory's **current** `gameImpls(gameType)`, but does not retain a historical implementation
   identity or read any artifact hashes.
2. `proof_manager.rs` places the game ending block in `schedule_l2_block_number` for both proof
   paths. Its TEE request uses `..Default::default()`, so `image_hash` is zero even though the
   historical game proxy exposes the expected hash.
3. Prover-service stores the request and currently filters claims only by proof type/backend.
   #4265 adds an exact integer version match and a pending-jobs-by-version gauge.
4. Shared `base-proof-worker` creates claim requests for both Nitro and ZK. On current main it has
   no version/capability field.
5. Nitro host builds witnesses, then the enclave derives `keccak256(PCR0)`, constructs the TEE
   journal, and signs it. `NitroEnclavePool` selects by registration and availability, not request
   image hash. Its registration guard calls `isValidSigner`, which compares against the factory's
   **current** image; that would reject a still-registered old-image signer needed by an old game.
6. zk-host embeds the range and aggregation ELFs at image build time and derives both verification
   keys at startup. They are not runtime-selectable today.
7. The SP1 range program commits `BootInfoStruct`; the aggregation program checks equal config and
   schedule IDs, verifies range proofs with the range vkey, emits the range vkey as `imageHash`,
   and produces a proof verified onchain against the aggregation vkey.

## Known verifier eras

| Era | Contract selection | Journal difference | Worker requirement |
| --- | --- | --- | --- |
| No schedule ID | AggregateVerifier before base/contracts #359 | Journal ends at TEE image hash or ZK range hash | Old journal encoder/program; no schedule bytes |
| Full schedule ID | base/contracts #359 / base/base #3965 | Same fields plus the full `ProtocolVersions.scheduleId()` snapshotted at L1 game initialization | Schedule-aware encoder plus the historical rollup-config snapshot that reproduces that exact value |
| Activated prefix | base/contracts #384 / base/base #4209 | Same layout as full-schedule, but value is `activatedScheduleId(game L2 timestamp)` | Prefix-aware encoder/program and the game ending L2 block |

The full-schedule and activated-prefix eras cannot be distinguished from `scheduleId()` alone:
both expose it and both may return zero. Historical activated-prefix games expose additional public
immutables such as `L2_BLOCK_TIME`, `L2_GENESIS_BLOCK_NUMBER`, and `L2_GENESIS_TIMESTAMP`. Probe
those only for legacy classification; add an explicit protocol identifier for future contracts.

### Full-schedule games are classified but not proven

The scanner recognises the full-schedule era and then **skips those games**, incrementing
`base_challenger_unsupported_schedule_games_total`. Recognising the era is what makes the routing
correct; proving it is a separate capability this phase does not have.

A proof request carries only `schedule_l2_block_number`, which reproduces an activated prefix. It
cannot express a `scheduleId` snapshotted at L1 game initialization — that needs the historical
rollup-config snapshot listed in the table above, plumbed through the request and understood by the
worker. Routing a full-schedule game without it would send a request that looks like a no-schedule
job, produce a journal committing the wrong schedule, and fail on-chain after a full proof run.

Skipping is therefore the honest failure: the game is unchallenged either way, and this way the
counter says so instead of a prover job burning and a submission reverting. Closing the gap means
adding the snapshot to the request contract, not changing the routing.

## Can the expected hashes be recovered?

Yes. No new getters are required to recover the current three eras' proof commitments.
`AggregateVerifier` has public immutable getters for:

- `CONFIG_HASH()`
- `TEE_IMAGE_HASH()` and `TEE_VERIFIER()`
- `ZK_RANGE_HASH()`, `ZK_AGGREGATE_HASH()`, and `ZK_VERIFIER()`
- `PROTOCOL_VERSIONS()` and activated-era L2 timestamp anchors
- `scheduleId()` on the two schedule-aware eras

Call these on the **game proxy**, not only on `factory.gameImpls(gameType)`. The clone delegates to
the implementation that created that game, so its immutable getters describe the historical game.
The current Rust `AggregateVerifierClient` omits these bindings; that is the missing in-repo surface.

Using the implementation address directly is harder than using its exposed commitments. Factory
`gameImpls(gameType)` is mutable and returns only the current implementation. `gameAtIndex` returns
game type, timestamp, and proxy, not the historical implementation. Recovering the exact address
would require parsing clone-with-immutable-args bytecode or maintaining a creation-block registry.

For future deployments, add an immutable `proofProtocolId() -> bytes32` calculated from a canonical
descriptor at deployment time. Keep the individual getters for auditability. Do not use
`version()`: the known incompatible AggregateVerifier eras all report `0.1.0`.

## Deployment system

Production fleet orchestration is in the separate `protocols/base-proofs` repository:

| Component | Current deployment shape | Version hook today |
| --- | --- | --- |
| prover-service | One Argo Rollout pod per network on EKS, backed by a two-node Aurora PostgreSQL cluster; Sif pipeline | Dockerfile pins a base/base commit; Helm supports arbitrary env vars but no protocol version |
| zk-host | One Argo Rollout pod per network on EKS; separate build/promote pipeline; shared SP1 cluster has independent CPU/GPU replica counts | Dockerfile pins base/base and embeds ELFs; config-service/envmapper can supply a new version env var |
| Nitro host/enclave | Odin deploy to EC2, one service/configuration per network; host and enclave are separate BaldurECR build targets | Separate host/enclave base/base pins; runtime values live in Odin/Config Service; entrypoint accepts one `VSOCK_CID` on master |

The Helm resource names and pipeline configuration names are singular (`zk-host`,
`prover-service`, `prover-nitro-$network`), so setting `replicaCount > 1` only creates identical
workers. N differentiated fleets require distinct configuration/deployable names or a chart loop
over a `fleets` list. Nitro requires the equivalent Odin configurations/ASGs. These are changes in
this repo plus external Config Service values; base/base alone cannot deploy the fleets.

The Dockerfiles currently pin components to different base/base commits, and open base-proofs #407
exists specifically to realign a no-schedule stack. This is direct evidence that a deployable
artifact manifest must bind the route ID to the prover-service schema, host binary, enclave PCR0,
and both SP1 vkeys instead of relying on "latest".

base/base now has a standalone Docker Compose prover stack from
[#4301](https://github.com/base/base/pull/4301), contrary to the older inventory. It is useful for
local ZK validation only: one Postgres, one prover-service, and one zk-host, with no Nitro fleet or
production lifecycle machinery.

## Options

### A. Opaque integer route ID backed by a capability descriptor — recommended now

Restore #4265's DB migration and exact-match queries after rebasing onto #4144. Add a reviewed
descriptor registry to challenger configuration:

```text
route_id -> descriptor, artifact manifest, historical full-schedule config (when needed)
```

The scanner reads the game proxy, constructs the descriptor, and fails closed if it has no unique
mapping. It sets `request_protocol_version = route_id`, the historical TEE image hash, and the
schedule input required by that mode. Nitro and ZK workers receive `--protocol-version` /
`PROVER_PROTOCOL_VERSION` at runtime and announce it through shared job discovery.

The runtime ID is a claim filter, not magic compatibility. A worker must validate at startup that
its actual PCR0/vkeys/mode match the manifest for the declared ID. A current binary labeled `0`
does not become a legacy encoder. Old binaries that omit the new claim field continue to announce
zero through serde defaults and may remain running unmodified.

Pros: smallest change, preserves the proven SQL design, supports arbitrary N, works with old
binaries, and keeps high-cardinality hashes out of metric labels. Cons: requires a controlled
mapping and artifact validation; `u32` is not itself cryptographic identity.

### B. Route directly by AggregateVerifier implementation address

Persist an address instead of `protocol_version`, and map each historical implementation to an
artifact descriptor.

Pros: unique deployment provenance and no manual integer allocation. Cons: the factory exposes
only its current implementation; extracting the historical address is nonstandard; equivalent
capabilities at two addresses split capacity; proof-kind-only changes duplicate unrelated fleets;
and this discards #4265's integer/SQL foundation. Use implementation address as registry metadata,
not the queue key.

### C. Allocate a new dispute game type for every verifier protocol

Never replace the implementation behind an existing game type. Map stable `gameType` values to
capabilities and route IDs.

Pros: the version boundary is explicit onchain, historical lookup is trivial, and upgrades cannot
silently reinterpret an old type. Cons: every proof-affecting hotfix needs contract deployment,
factory registration, respected-game migration, config changes, and operational coordination.

Recommendation: adopt this as the policy for future verifier upgrades, while option A handles the
already-deployed eras and remains the queue protocol.

## Observability and retirement

Restore #4265's `pending_jobs{protocol_version}` gauge, but do not use it alone as the retirement
signal. No pending job can exist until a bad game is discovered, while an open game still represents
latent challenge demand.

Add:

- `open_games{protocol_version}` in the challenger, counting every in-progress game still inside
  its challenge window by resolved descriptor, not only games currently selected for an action;
- a prover-service worker capability lease updated by `getNextProof`, persisted as
  `(worker_id, proof kind, route ID, capabilities, last_seen)`;
- `active_workers{protocol_version,proof_type}` with a short expiry;
- an alert when open games or pending jobs are nonzero and the corresponding active worker count is
  zero.

Retirement procedure:

1. Stop assigning new game implementations to the old route.
2. Keep at least one matching Nitro and ZK worker available for the full challenge window.
3. Require `open_games == 0` and `pending_jobs == 0` for the route for a reviewed safety interval.
4. Repoint that fleet's resources to the newest artifact manifest; no teardown automation is needed.
5. Keep the descriptor/artifact record for audit and emergency replay.

## Deploy-on-request

Defer deploy-on-request for the first rollout. Nitro boot/registration, image availability, ZK ELF
setup, and SP1 backend startup add cold-start latency directly against the challenge deadline. The
standing-fleet design is already needed to establish reliable startup and proof-duration SLOs.

Revisit on-demand old versions only after measuring those SLOs and building a deadline-aware
controller that keeps a safety margin, deduplicates concurrent starts, and falls back to warm
capacity. Scaling an old worker control plane to zero is not safe merely because its pending-job
gauge is zero.

## Phase 1 implementation

### base/base

- Prover-service persists an arbitrary `u32` route ID and claims jobs matching any version the
  worker announces.
- Nitro and ZK workers require `PROVER_PROTOCOL_VERSION` at runtime; shared job discovery announces
  it on every claim.
- Multi-version fleets are Nitro-only in this phase. The Nitro host takes a comma-separated
  `PROVER_PROTOCOL_VERSION` and one fleet serves every version it announces; zk-host takes a single
  version, so N versions means N zk-host deployments. See
  [ZK multi-version](#zk-multi-version-deferred) for the cost of closing that gap.
- The proposer requires `BASE_PROPOSER_PROOF_PROTOCOL_VERSION`.
- The challenger requires one or more comma-separated mappings in
  `BASE_CHALLENGER_PROOF_PROTOCOL_VERSION`, formatted as
  `<bytes32 fingerprint>=<u32 route ID>`.
- The challenger reads commitments from each historical game proxy, fails closed on unknown
  fingerprints, sends the historical TEE image hash, and selects schedule inputs by era.
- Nitro selects only registered enclaves whose recorded image hash matches the proof request.
- Pending jobs and open actionable games are reported by route ID.

## ZK multi-version (deferred)

Phase 1 gives the Nitro host a comma-separated `PROVER_PROTOCOL_VERSION` so one fleet claims
several versions. zk-host keeps a single scalar version. This section records why the asymmetry is
deliberate and what closing it would cost.

### Why the asymmetry is not a capability gap

ZK already covers N versions today, by running N deployments. The `zk-host` chart takes a
`workers[]` list, each entry with its own `protocolVersion` and `replicaCount`, and zeronet already
runs two (`v0` and `v1`). So "serve two versions" is a solved problem on the ZK side — it costs one
extra Rollout.

The reason that answer does not transfer to Nitro is the shape of the compute:

- A zk-host pod is a thin claimer and orchestrator. It requests 8Gi/1cpu (limit 32Gi/2cpu) and
  offloads the actual proving to the **shared** SP1 cluster behind `SP1_CLUSTER_API_ENDPOINT`. An
  extra per-version deployment duplicates the orchestrator, not the prover hardware.
- A Nitro fleet is an Odin/EC2 auto-scaling group of instances running enclaves, plus per-instance
  attestation and registration against `TEEProverRegistry`, all sharing exactly one ALB target
  group (the only ARN the registrar discovers). An extra per-version fleet duplicates the proving
  hardware and adds a registration lifecycle.

So multi-version-per-fleet buys a lot for Nitro and comparatively little for ZK. That is the whole
justification for shipping it on one side first.

### The hard constraint: one ELF pair per binary

`base-proof-succinct-elfs` embeds the programs at compile time:

```rust
pub const AGGREGATION_ELF: &[u8] = include_bytes!(env!("AGGREGATION_ELF_PATH"));
pub const RANGE_ELF_EMBEDDED: &[u8] = include_bytes!(env!("RANGE_ELF_EMBEDDED_PATH"));
```

A zk-host binary therefore has exactly one `(range vkey, aggregation vkey)` pair, and the
capability fingerprint mixes both into the version identity:

```text
keccak256("base-proof-protocol-v1" || schedule_kind || schedule_id || config_hash
          || tee_image_hash || zk_range_hash || zk_aggregate_hash)
```

That splits the work into two very different problems.

**Case 1 — versions differing only outside the ZK commitments.** A TEE image rotation, a config
change, or a schedule-era change mints a new version while `zk_range_hash` and `zk_aggregate_hash`
stay identical. One binary is genuinely valid for both versions, and announcing both is sound. This
is the common case and the only one worth automating.

**Case 2 — versions with different ZK programs.** One binary cannot serve both, full stop. Doing so
means embedding multiple ELF sets, selecting per job, holding several prover instances, and
registering every program set with the SP1 cluster. This is a large change with a real memory and
artifact-management cost, and it is explicitly out of scope. For case 2, run a fleet per version —
which is what the chart already does.

### What case 1 actually costs

The Rust diff is nearly free, because the shared plumbing is already multi-version from this phase:

- the `GetNextProofRequest.protocol_versions` wire field is already `Vec<u32>`;
- `JobDiscoveryConfig::with_protocol_versions` already sorts, dedups and falls back to `[0]`;
- the ZK branch of the claim query already matches `request_protocol_version = ANY($n::bigint[])`.

Only the host edge is scalar. Restoring the vector is roughly 20 lines: `ZkHostConfig`'s field and
setter go back to `Vec<u32>`, and the zk-host CLI arg regains `value_delimiter = ','` with
`num_args = 1..`, exactly mirroring `bin/prover/nitro-host/src/cli.rs`.

The cost is everything around that:

1. **A startup cross-check, and the manifest it depends on.** Nothing today binds a declared
   version to the artifacts the process actually runs. A fleet that announces a version whose
   `zk_range_hash` does not match its embedded ELF will claim those jobs and then fail every one —
   and because a failed job is logged and dropped rather than nacked, the job returns when the
   lease expires and the mismatched fleet can claim it again. It fails closed, but it also starves
   the version. The fix is for the host to derive its own vkeys at boot, look up each announced
   version's expected hashes, and refuse to start on mismatch. That requires the **version →
   artifact manifest** listed under "Remaining fleet work", which does not exist yet. This is the
   real dependency; the multi-version flag is not safe to expose without it. Note the same hazard
   already exists for Nitro, where it is bounded by `select_enclaves_for_image` rejecting a
   non-matching PCR0 before proving.
2. **Helm label plumbing.** `zk-host-rollout.yaml` and `zk-host-pdb.yaml` stamp
   `base.org/proof-protocol-version: {{ ... | quote }}`, and the PDB selects on it. Kubernetes label
   values cannot contain commas, so a multi-version value breaks rendering and PDB selection. The
   label needs a sanitized form (for example `v0-1`) or must move out of the selector, and
   `verify-helm-charts.yml`, which asserts one `PROVER_PROTOCOL_VERSION` per worker entry, needs
   updating with it.
3. **Deciding what a fleet may announce.** Case 1 soundness is a property of the manifest, not of
   the operator's intent. Whoever writes the version list has to know that two versions share ZK
   commitments. Without the manifest that is tribal knowledge, which is the failure mode item 1
   guards against.

### Recommendation

Do it after the artifact manifest lands, not before, and only for case 1. Sequenced that way it is
a ~20-line Rust change plus the Helm label fix, with the cross-check falling out of the manifest
work that is already planned. Done before the manifest, it is a foot-gun that trades one extra
8Gi pod for a silent way to starve a protocol version.

## Remaining fleet work

- Add versioned zk-host Sif configurations and Nitro Odin configurations with
  `PROVER_PROTOCOL_VERSION` and immutable artifact pins. One zk-host worker entry per version; a
  Nitro fleet may announce several.
- Publish an artifact manifest containing base/base commit, prover-service schema compatibility,
  Nitro PCR0, range vkey, aggregation vkey, and descriptor hash.
- Keep one shared prover-service per network unless a future server wire break requires a staged
  service rollout; do not create N databases merely because there are N worker versions.
- Add worker capability leases and Datadog alerts joining active workers, open games, and pending
  jobs. Pending/open gauges land in Phase 1; active-worker expiry belongs with fleet wiring.
- Confirm historical full-schedule rollup-config snapshots and artifact images before enabling that
  route.
- Choose initial warm capacity per network. Deploy-on-request remains deferred.
