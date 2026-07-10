# P2P Block-Latency Measurement — Deployment Runbook

> **THROWAWAY / ONE-OFF.** This is a lightweight kit for a single P0 measurement:
> how long does a canonical block take to arrive over Base CL gossip at observer
> nodes in different regions? It is intentionally *not* internal infra — it runs
> on cheap external VMs (Vultr), writes an append-only CSV per node, and is meant
> to be torn down when the measurement is done. Do not productionize this.

## What we are measuring

Each observer is a stock `base-consensus` node that stays at the tip of Base
mainnet, joins the CL gossip mesh, and receives unsafe blocks over gossipsub.
Two **new** CLI flags being added to `base-consensus` cause it to append one row
per received block to a CSV:

```
--p2p.latency.log <path>       (env BASE_NODE_P2P_LATENCY_LOG)
--p2p.latency.region <name>    (env BASE_NODE_P2P_LATENCY_REGION)
```

> **NEW FLAGS — TODO / not-yet-landed.** As of this kit these two flags are being
> *added* to `base-consensus`; they do not yet exist in the CLI I inspected.
> The remainder of the run command is grounded in the real, existing flags (see
> `crates/consensus/cli/src/p2p.rs`, `l1.rs`, `l2.rs`, `chain.rs`, `node.rs`,
> `app.rs`). When the flags land, confirm their exact spelling/env names against
> the p2p args struct (`crates/consensus/cli/src/p2p.rs`) and update
> `run_observer.sh` if they differ.

The latency timestamp is recorded at **gossip-receive time, before engine
insertion**. That means a fully-synced execution layer may be unnecessary for
the metric itself — the block-arrival timestamp does not depend on the EL
applying the block. **Validate this on ONE node first**: bring up a single
observer, confirm it is a healthy gossip mesh member and that CSV rows are
landing, *then* replicate to the other five regions. See the CL-only trial note
under "Node sizing".

## Regions & provider

One observer per region. Provider suggestion: **Vultr** — it is the one cheap
provider that covers all six locations. Alternatives noted per row (**OVH** and
**Latitude.sh** are good fallbacks but do not cleanly cover every region).

| Label          | Location                     | Vultr region        | Alt provider          |
| -------------- | ---------------------------- | ------------------- | --------------------- |
| `us-east`      | Ashburn / NJ (US East)       | Newark / New Jersey | OVH (Vint Hill, VA)   |
| `us-west`      | Silicon Valley / LA          | Silicon Valley      | Latitude.sh (LA)      |
| `eu-central`   | Frankfurt                    | Frankfurt           | OVH / Latitude.sh (FRA) |
| `eu-north`     | Stockholm (≈ Finland)        | Stockholm           | OVH (limited N. EU)   |
| `ap-northeast` | Tokyo                        | Tokyo               | OVH (Singapore alt)   |
| `ap-southeast` | Sydney                       | Sydney              | Latitude.sh (Sydney)  |

The `<region>` label above is exactly the string to pass to
`--p2p.latency.region` / `REGION` — keep it consistent so the collected CSVs and
analysis line up.

## Node sizing

Target: a **pruned full Base node** at the tip.

| Resource | Recommendation                                        |
| -------- | ----------------------------------------------------- |
| vCPU     | 4–8                                                   |
| RAM      | 16–32 GB                                              |
| Disk     | ~500 GB – 1 TB NVMe (pruned full node)                |
| Network  | Unmetered / high egress cap; low-latency peering      |

**CL-only trial option (lighter/cheaper).** Because latency is recorded at
gossip-receive *before* engine insertion, you can trial a much smaller box that
runs *only* `base-consensus` (no local EL) and points `--l2-engine-rpc` at a
throwaway / stub EL. The node will still join the mesh and receive blocks; the
CSV rows are written regardless of whether the EL keeps up. **Validate mesh
membership and CSV rows on one node before committing to sizing for all six.**
If the CL-only trial works, the other five can be small (2–4 vCPU / 8 GB) and
skip the ~1 TB EL disk. If you find you *do* need a synced EL (e.g. the new
latency hook fires after engine insertion — confirm when the flags land), fall
back to the full sizing above.

Each observer needs, regardless of sizing:
- An **L1 execution RPC** (`--l1-eth-rpc`) and **L1 beacon** (`--l1-beacon`).
  Use a hosted L1 endpoint (e.g. a provider RPC) — do not run L1 on these boxes.
- An **L2 engine endpoint** (`--l2-engine-rpc`) + **JWT** for the local (or stub) EL.

## Timeline

Times are relative to **T0** = the moment the measurement window opens. Run the
window for **7 days**; **48–72h is the hard minimum** if the schedule slips.

### T-2d → T-1d — Provision & prepare (per node)
1. Provision the VM in each region (see table). Ubuntu/Debian LTS.
2. Run `setup_chrony.sh` on every node. Confirm `chronyc tracking` reports a
   **sub-few-ms** offset — this bounds the cross-observer metric, so a node whose
   clock is off by tens of ms is useless. Re-run until offset is small and stable.
3. Install the `base-consensus` binary and drop `run_observer.sh`,
   `base-observer.service` in place. Set the env (REGION, endpoints, JWT).
4. Start the node and let it **snap-sync to tip**. For a CL observer this means
   discovering peers and joining the gossip mesh; for a full node also let the EL
   snap-sync.
5. **Confirm healthy mesh membership** with the latency flags set:
   - CL RPC (`--rpc.addr` / `--port`, default `9545`) or the metrics endpoint
     (`--metrics.enabled`, default port `9090`) shows a healthy peer count and
     gossip mesh (target mesh degree D=8, see `p2p.gossip.mesh.d`).
   - The CSV at `LOG_PATH` exists and rows are being appended.
   Do this on **one** node first (the validation node) before scaling to six.

### T0 — Window opens
- All six nodes healthy mesh members, chrony offsets small, CSVs growing.
- Record wall-clock T0 and the L2 block number at T0 for later trimming.

### T0 + 24h — Checkpoint
1. Run `collect_logs.sh` to pull **partial** CSVs to `./collected/<region>.csv`.
2. Run the analysis **dry-run** on the partials (sanity: parseable, monotonic
   block numbers, plausible inter-region deltas).
3. Confirm on every node:
   - Rows are landing (CSV row count increasing at ~block cadence).
   - `chronyc tracking` offset still **< a few ms**. A node that drifted is
     suspect for the whole window — note it.
4. Fix/restart any unhealthy node before the bulk of the window elapses.

### T0 + 7d — Close & collect
1. Stop the window (leave nodes running until CSVs are safely collected).
2. Run `collect_logs.sh` for the final pull.
3. Analyze: per-region arrival-time distributions, cross-region deltas relative
   to the earliest-observing node per block, tail latencies. Trim to the
   `[T0, T0+7d]` block-number range recorded above.
4. Tear down the VMs. This kit is throwaway — do not leave it running.

## Files in this kit

| File                    | Purpose                                                        |
| ----------------------- | -------------------------------------------------------------- |
| `README.md`             | This runbook.                                                  |
| `setup_chrony.sh`       | Install/enable chrony on Ubuntu/Debian; print clock offset.    |
| `run_observer.sh`       | Launch the `base-consensus node` observer with latency flags.  |
| `base-observer.service` | systemd unit template running `run_observer.sh`.               |
| `collect_logs.sh`       | rsync/scp each region's CSV to `./collected/<region>.csv`.     |
| `hosts.example`         | Example hosts file for `collect_logs.sh`.                      |

## Confirmed real flags (grounding)

From the CLI I inspected (file:line):
- Subcommand: `base-consensus node` — `crates/consensus/cli/src/app.rs:52`.
- `--chain` / `-n` (L2 chain id/name, default `8453`) — `chain.rs:24`
  (env `BASE_NODE_NETWORK`).
- `--l1-eth-rpc` (alias `--l1`, env `BASE_NODE_L1_ETH_RPC`) — `l1.rs:11`.
- `--l1-beacon` (alias `--l1.beacon`, env `BASE_NODE_L1_BEACON`) — `l1.rs:23`.
- `--l2-engine-rpc` (alias `--l2`, env `BASE_NODE_L2_ENGINE_RPC`) — `l2.rs:17`.
- `--l2.jwt-secret` (env `BASE_NODE_L2_ENGINE_AUTH`, path to hex) — `l2.rs:21`.
- `--p2p.listen.tcp` default `9222` — `p2p.rs:106`; `--p2p.listen.udp` default
  `9223` — `p2p.rs:109`; `--p2p.advertise.ip` — `p2p.rs:89`.
- `--p2p.bootnodes` (env `BASE_NODE_P2P_BOOTNODES`) — `p2p.rs:235`;
  `--p2p.bootstore` — `p2p.rs:207`. Built-in bootnodes are used by default; the
  bootstore persists discovered peers.
- `--p2p.gossip.mesh.d` default `8` — `p2p.rs:140` (mesh degree to sanity-check).
- `--metrics.enabled` / `--metrics.port` default `9090` —
  `crates/utilities/cli/src/macros.rs:38` (via `define_metrics_args!`).
- `--rpc.addr` default `0.0.0.0` / `--port` (alias `--rpc.port`) default `9545` —
  `rpc.rs:25,28`.

## Assumptions / open TODOs
- **`--p2p.latency.log` / `--p2p.latency.region` are NOT yet in the CLI.** They
  are the two new flags this measurement depends on. Confirm exact names + envs
  when they land (`crates/consensus/cli/src/p2p.rs`).
- Bootnodes: relying on the built-in defaults for chain `8453`. If discovery is
  slow, pass explicit `--p2p.bootnodes` (`p2p.rs:235`) or a bootnodes file
  (`--p2p.bootnodes-file`, `p2p.rs:246`).
- L1 RPC + beacon and L2 engine endpoints are operator-supplied (hosted or a
  local/stub EL). Not provisioned by this kit.
- Whether a synced EL is truly optional depends on where the new latency hook
  fires relative to engine insertion — validate on one node first.
