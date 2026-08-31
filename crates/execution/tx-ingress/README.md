# Base Transaction Ingress

Private streaming transaction ingress for Base mempool nodes.

The service accepts EIP-2718 encoded transactions over a persistent bidirectional gRPC stream and
submits each transaction through the same admission path as `eth_sendRawTransaction`. Responses may
arrive out of order and are correlated with the request ID supplied by the client.

Set `--tx-ingress.addr <IP:PORT>` on a Base execution node to enable the service. Request IDs are
scoped to one live stream and require no durable state. This private endpoint is intended for
trusted proxyd instances. The server intentionally does not limit connections or concurrent
admissions; every received transaction is submitted independently.

## Local Benchmark

The crate includes a fixed-workload benchmark that compares individual `eth_sendRawTransaction`
HTTP requests with submissions over one persistent bidirectional gRPC stream. Each transport gets
a fresh in-process node with identical funded genesis state and the same pre-signed transaction
corpus. Node startup, signing, and connection warm-up are excluded from the measured interval.

```bash
RUST_LOG=error cargo bench -p base-tx-ingress --bench transaction_ingress
```

The default workload submits 5,000 transactions from 5,000 unique senders at 1, 64, 256, and
1,024 concurrent in-flight submissions. Configure it with environment variables:

```bash
RUST_LOG=error \
TX_INGRESS_BENCH_TRANSACTIONS=5000 \
TX_INGRESS_BENCH_SENDERS=5000 \
TX_INGRESS_BENCH_IN_FLIGHT=64,256,1024 \
TX_INGRESS_BENCH_REPETITIONS=3 \
TX_INGRESS_BENCH_REQUEST_TIMEOUT_MS=5000 \
cargo bench -p base-tx-ingress --bench transaction_ingress
```

Results are printed as CSV with total throughput, mean latency, p50, p95, p99, p99.9, accepted and
rejected submission counts, and the first observed error. Transport order alternates between
repetitions to reduce ordering bias.

This benchmark measures closed-loop saturation against one local node. It does not model proxyd
routing, JSON-RPC batch decomposition, network delay, or an open-loop arrival rate. Those belong in
a separate multi-process load test built on the transport and admission baselines reported here.
