# Validity-transaction incident scenarios

Load profiles that reproduce the traffic behind the October–November 2026 validity incidents,
for running against a Denim network (Base Sepolia) before mainnet activation. Each run pairs a
`validity-flood` workload with plain user transfers, so the result shows what the flood does to
ordinary users.

The flood models how market makers use `base_sendRawTransactionValidity` on mainnet. Every
sender holds one pending nonce and re-quotes it: one add, then same-nonce replacements with
bumped fees, then a wait for the block-number expiry before the next add. Predicates almost
never pass (mainnet matches about 0.15% of evaluations).

| Scenario | Incident | Shape |
| --- | --- | --- |
| `s1-resting-flood-15s` | Nov 2 (post-mitigation limits) | 16 predicates, 15s lifetime, 1 add + 2 replacements per window, ramp 500 → 4,000 adds/s |
| `s2-resting-flood-60s` | Nov 2 (pre-mitigation 60s lifetime) | Gate-only transactions, 60s lifetime, resting set driven past the sequencer's 40k validity-pool cap |
| `s3-wakeup-storm` | Herd inclusion | Every transaction also waits for the next multiple of 25 blocks, so thousands become includable in one block |
| `s4-pool-bytes` | Oct 24 (pool fill) | 2,400-byte calldata to reach the validity pool's byte cap |
| `s5-resting-max-members` | Nov 2 (resting walk) | Gate-only transactions, 15s lifetime, maximum resting-set size |
| `user-traffic-sepolia` | — | 20M gas/s of transfers (~950 tx/s), run alongside every scenario |

The predicates gate on `L1Block.sequenceNumber` (slot 3, low 64 bits) of the L1Block predeploy.
The system deposit writes that slot every block, so each parked transaction is woken every block
and nothing needs deploying.

## Running

```sh
cargo build --release -p base-load-tester-bin
export SEPOLIA_RPC=https://...          # submission endpoint; kept out of the repo
# funder key: ~/.sepolia-funder by default, or FUNDER_KEY_FILE=/path, or FUNDER_KEY=0x...
./run.sh baseline                       # user traffic only
./run.sh s1-resting-flood-15s           # USER_DURATION=6m overrides the user-traffic duration
```

`SEPOLIA_RPC=... ./summarize.py results/<run>` prints sequencer and mempool Datadog metrics
(needs the `pup` CLI) and per-minute block stats for a finished run.

Results go to `results/<scenario>-<timestamp>/`: the flood and user logs, `flood-summary.json`
and `user-summary.json`, plus timestamps for matching the run window in Datadog. Rendered
configs are deleted after a run because they contain the endpoint.

Drain senders afterwards with `base-load-tester rescue --seed <seed> --count <n>`. The flood
seeds are 4242 (60,000 senders) and 4343 (12,000); user traffic uses 777001 (200 senders).

## Caveats

- **Funder:** use a plain EOA. EIP-7702 delegated accounts are limited to one in-flight
  transaction, so batched funding is rejected.
- **Sepolia ingress:** two mempool nodes serve traffic, each on a 12-vCPU i3en.3xlarge. They
  reach ~85–90% host CPU at ~2.9k validity submissions/s in total and fall over between ~2.9k
  and ~4k/s. Above that, submissions time out and user
  transactions queue upstream of the sequencer. The builder only sees what the mempools
  forward.
- **Client:** a single client host tops out around 5–8k submissions/s. Check `late_ticks` in
  the flood log; a growing count means the client, not the network, is the limit.
