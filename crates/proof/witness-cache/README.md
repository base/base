# `base-witness-cache`

In-memory cache of `debug_executePayload` witnesses.

A sidecar asks the proof node for each new L2 block's execution witness as soon as the block is built, keeps the response in memory, and serves it to provers. Provers fall back to the proof node on a miss.

## Run

```sh
cargo run -p base-witness-sidecar -- \
  --l2-eth-url http://127.0.0.1:8545 \
  --l2-chain-id 8453 \
  --listen-addr 127.0.0.1:7400
```

Point a prover at it with `--witness-cache-url http://127.0.0.1:7400` or `WITNESS_CACHE_URL`.

Metrics listen on port 7401 unless `BASE_WITNESS_SIDECAR_METRICS_PORT` is set.

The default retention is 3600 blocks, about two hours at a 2 second block time. Uncompressed witnesses are about 15 megabytes, so that default is on the order of 50 gigabytes of RAM. The follower starts at the current head and does not backfill older blocks.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
