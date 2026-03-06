# Self-Hosted Proving Cluster

By default, op-succinct uses the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro) to generate proofs. If you want to run your own proving infrastructure instead, you can deploy a self-hosted [SP1 cluster](https://github.com/succinctlabs/sp1-cluster) and point op-succinct at it.

## Prerequisites

- A deployed SP1 cluster passing the fibonacci smoke test. Follow the [SP1 Cluster Kubernetes Deployment Guide](https://docs.succinct.xyz/docs/provers/setup/deployment/kubernetes) to set one up.
- RPC endpoints for your OP Stack chain (L1, L1 beacon, L2, L2 node).
- A gRPC endpoint for the cluster API. Exposing `api-grpc` via a LoadBalancer or Ingress is recommended — `kubectl port-forward` works for quick tests but is unreliable for long-running proofs.

## Cluster Resource Requirements

The cpu-node must be sized to handle both range proofs (CONTROLLER task) and aggregation proofs (PlonkWrap task). The coordinator schedules tasks based on weight — a task is only assigned to a worker if `current_weight + task_weight ≤ max_weight`.

| Task | Weight (GB) | Runs on |
|------|------------|---------|
| CONTROLLER | 16 | cpu-node |
| PlonkWrap (Plonk mode) | 32 | cpu-node |
| Groth16Wrap (Groth16 mode) | 14 | cpu-node |
| ProveShard | 4 | gpu-node |
| ShrinkWrap | 4 | gpu-node |

During aggregation, the CONTROLLER and PlonkWrap tasks briefly co-exist on the cpu-node, requiring at least 48 GB of weight capacity for Plonk mode (CONTROLLER=16 + PlonkWrap=32).

**Recommended cpu-node configuration:**

```yaml
cpu-node:
  resources:
    requests:
      memory: 64Gi
    limits:
      memory: 64Gi
  extraEnv:
    WORKER_MAX_WEIGHT_OVERRIDE: "48"
    WORKER_CONTROLLER_WEIGHT: "16"
```

`WORKER_CONTROLLER_WEIGHT` sets the weight for CONTROLLER tasks (default: 4 in code). Set it to 16 to reflect actual RAM usage (~16–19 GB with default splicing workers). `WORKER_MAX_WEIGHT_OVERRIDE` should be set to exactly 48 — this allows CONTROLLER (16) + PlonkWrap (32) to co-schedule, while preventing two PlonkWraps (32+32=64) from running simultaneously and OOMKilling the node.

```admonish warning
Do not set `WORKER_MAX_WEIGHT_OVERRIDE` higher than 48 on a 64Gi node. A value of 64 allows two PlonkWrap tasks (weight=32 each) to be co-scheduled, consuming ~64 GB of actual RAM and OOMKilling the pod. The value 48 ensures only one PlonkWrap runs at a time while still fitting alongside a CONTROLLER task.
```

## Quick Test: Range Proof

Before integrating with the full proposer, verify that your cluster can generate op-succinct proofs using the `multi` script.

### 1. Set up environment

Create a `.env` file in the op-succinct root:

```env
L1_RPC=<YOUR_L1_RPC>
L1_BEACON_RPC=<YOUR_L1_BEACON_RPC>
L2_RPC=<YOUR_L2_RPC>
L2_NODE_RPC=<YOUR_L2_NODE_RPC>
```

### 2. Connect to cluster services

**Recommended:** Expose `api-grpc` via a LoadBalancer for stable, long-lived connections:

```bash
# Patch the service to use a LoadBalancer (e.g., AWS NLB)
kubectl patch svc api-grpc -n sp1-cluster -p '{"spec":{"type":"LoadBalancer"}}'

# Get the external endpoint
kubectl get svc api-grpc -n sp1-cluster -o jsonpath='{.status.loadBalancer.ingress[0].hostname}'
```

**Alternative (quick local testing only):** Use port-forward. Note that `kubectl port-forward` can drop under sustained load — the client polls the cluster every 50ms for the entire proving duration.

```bash
kubectl port-forward svc/api-grpc 50051:50051 -n sp1-cluster
```

### 3. Validate witness generation

Run without `--prove` first to verify RPC connectivity:

```bash
RUST_LOG=info cargo run --bin multi --release -- --env-file .env
```

This auto-detects the latest finalized L2 block range (default: 5 blocks).
Use `--default-range <N>` to change the range size, or `--start <BLOCK> --end <BLOCK>` for an exact range.

This executes locally and prints execution stats. If this fails, fix RPC issues before using cluster time.

### 4. Generate a range proof

**With S3 artifacts (recommended):**

```bash
SP1_PROVER=cluster \
CLI_CLUSTER_RPC=http://<CLUSTER_LB_ENDPOINT>:50051 \
CLI_S3_BUCKET=<YOUR_S3_BUCKET> \
CLI_S3_REGION=<YOUR_REGION> \
RUST_LOG=info \
cargo run --bin multi --release -- --prove --env-file .env
```

**With Redis artifacts:**

```bash
SP1_PROVER=cluster \
CLI_CLUSTER_RPC=http://<CLUSTER_LB_ENDPOINT>:50051 \
CLI_REDIS_NODES="redis://:<YOUR_REDIS_PASSWORD>@<REDIS_HOST>:6379/0" \
RUST_LOG=info \
cargo run --bin multi --release -- --prove --env-file .env
```

```admonish warning
Redis artifacts have a hardcoded 4-hour TTL. For large proofs that take longer, use S3 instead to avoid artifacts expiring mid-prove.
```

```admonish info
Start with a small range (5 blocks). A 5-block range proof typically completes in ~8 minutes on a single GPU worker.
```

A successful run produces output like:

```
INFO using s3 artifact store
INFO connecting to http://<CLUSTER_LB_ENDPOINT>:50051
INFO upload took 182ms, size: 2307656
INFO Successfully created proof request cli_<timestamp>
INFO Proof request for proof id cli_<timestamp> completed after ~475s
INFO Completed after ~475s
```

Proofs are saved to `data/<chain_id>/proofs/<start_block>-<end_block>.bin`.

## Running the Proposer

Before running a proposer, complete the relevant setup guide to deploy contracts and configure your environment:

- **Fault proofs**: [Quick Start Guide](../fault_proofs/quick_start.md)
- **Validity proofs**: [Contract Deployment](../validity/contracts/deploy.md)

Then add the following cluster variables to your proposer environment file (`.env` for [validity](../validity/proposer.md), `.env.proposer` for [fault proofs](../fault_proofs/proposer.md)):

```env
SP1_PROVER=cluster
CLI_CLUSTER_RPC=http://<CLUSTER_LB_ENDPOINT>:50051
CLI_S3_BUCKET=<YOUR_S3_BUCKET>
CLI_S3_REGION=<YOUR_REGION>
RUST_LOG=info
```

If using Redis instead of S3, replace `CLI_S3_BUCKET` + `CLI_S3_REGION` with `CLI_REDIS_NODES`.
You must set exactly one artifact store — setting both (or neither) will panic.

```admonish note
Self-hosted cluster mode is an alternative to the Succinct Prover Network. You do **not** need a `NETWORK_PRIVATE_KEY`.
Also there's no need to set `OP_SUCCINCT_MOCK=true` or `MOCK_MODE=true` — cluster mode uses real proving.
```

### Tuning for large proofs

For proofs that may take longer than 4 hours, increase the proving timeout:

```env
PROVING_TIMEOUT=21600  # 6 hours (default: 14400 = 4 hours)
```

This controls both the client-side timeout and the deadline sent to the cluster coordinator.

## Monitoring

Monitor cluster proof progress with these commands:

```bash
# Overall stats (gpu_queue, cpu_queue, active proofs)
kubectl logs deploy/coordinator -n sp1-cluster --tail=3 | grep GetStatsResponse

# Which tasks are assigned to which workers
kubectl logs deploy/coordinator -n sp1-cluster --tail=3 | grep "worker"

# CPU worker activity (CONTROLLER, PlonkWrap)
kubectl logs deploy/cpu-node -n sp1-cluster --tail=20

# GPU worker activity (ProveShard, ShrinkWrap)
kubectl logs deploy/gpu-node -n sp1-cluster --tail=20

# Resource usage
kubectl top pod -n sp1-cluster
```

Key fields in `GetStatsResponse`:
- `gpu_queue`: GPU tasks waiting — drops to 0 when shard proving is done.
- `cpu_queue`: CPU tasks waiting — should be 0 unless PlonkWrap is queued.
- `active_proofs`: Number of proofs being processed.
- `cpu_utilization_current/max`: Current vs maximum weight on cpu workers.

## Troubleshooting

### Proof request hangs

1. Verify `RUST_LOG=info` is set — without it, the CLI produces no output.
2. Check coordinator and worker logs:
   ```bash
   kubectl logs -l app=coordinator -n sp1-cluster
   kubectl logs -l app=gpu-node -n sp1-cluster
   ```
3. Verify the artifact store (S3 or Redis) is reachable from workers.

### "cluster proof failed" error

Check that `CLI_CLUSTER_RPC` is reachable and the API pod is running:

```bash
kubectl get pods -n sp1-cluster
kubectl logs -l app=api -n sp1-cluster
```

### Proof times out

The default `PROVING_TIMEOUT` is 14400 seconds (4 hours). If your proof needs more time (large block ranges, few GPU workers), increase it. The cluster also has a per-task timeout of 6 hours (`TASK_TIMEOUT`) on the worker node — individual shard tasks that exceed this are retried on another worker.

### Aggregation proof stuck (`cpu_queue` not draining)

If range proofs complete but the aggregation proof hangs with `cpu_queue: 1` in coordinator logs:

1. Check coordinator stats:
   ```bash
   kubectl logs deploy/coordinator -n sp1-cluster --tail=3 | grep GetStatsResponse
   ```
2. If `cpu_queue: 1` persists with `gpu_queue: 0`, the PlonkWrap task likely can't be scheduled due to insufficient weight capacity.
3. Verify `cpu_utilization_max` is at least 48 (for Plonk) or 30 (for Groth16). If it shows 32, increase `WORKER_MAX_WEIGHT_OVERRIDE`:
   ```bash
   kubectl set env deploy/cpu-node -n sp1-cluster WORKER_MAX_WEIGHT_OVERRIDE=48
   ```
   Also ensure the cpu-node has enough memory (at least 64Gi) and `WORKER_CONTROLLER_WEIGHT=16`. See [Cluster Resource Requirements](#cluster-resource-requirements).

### cpu-node OOMKilled (CrashLoopBackOff)

If the cpu-node enters CrashLoopBackOff with exit code 137:

```bash
kubectl describe pod -l app=cpu-node -n sp1-cluster | grep -A3 "Last State"
```

Common causes:

1. **Two PlonkWraps co-scheduled**: If `WORKER_MAX_WEIGHT_OVERRIDE` is set too high (e.g., 64), two PlonkWrap tasks (weight=32 each) can run simultaneously, consuming ~64 GB of actual RAM. Set `WORKER_MAX_WEIGHT_OVERRIDE=48` to prevent this — only one PlonkWrap will run at a time.
2. **CONTROLLER task exceeds memory**: Large block ranges require more memory during execution/splicing. Reduce `RANGE_PROOF_INTERVAL` in the proposer to produce smaller proofs, or increase the cpu-node memory limit.

To increase cpu-node memory:

```bash
kubectl patch deploy cpu-node -n sp1-cluster --type='json' \
  -p='[{"op":"replace","path":"/spec/template/spec/containers/0/resources/limits/memory","value":"64Gi"},
       {"op":"replace","path":"/spec/template/spec/containers/0/resources/requests/memory","value":"64Gi"}]'
```

Always keep `WORKER_MAX_WEIGHT_OVERRIDE` at 48 regardless of memory size — this prevents co-scheduling of two PlonkWraps. See [Cluster Resource Requirements](#cluster-resource-requirements).
