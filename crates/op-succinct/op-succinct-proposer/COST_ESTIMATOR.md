# Cost Estimator

To run the cost estimator, first build the native host runner.

```bash
just
```

Then start the host in the background.
```bash
docker build -t span_batch_server -f op-succinct-proposer/Dockerfile.span_batch_server .
docker run -p 8080:8080 -d span_batch_server
```

Then, run the cost estimator.

This command will execute `op-succinct` as if it's in production. Specifically, it will divide the entire block range
into smaller ranges optimized along the span batch boundaries. Then, it will aggregate the results to get the total
statistics.

```bash
cargo run --bin cost_estimator --release -- --start 16230000 --end 16230100
```