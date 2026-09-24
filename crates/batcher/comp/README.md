# `base-comp`

Channel compression for Base.

## Overview

`CompressionStream::append` incrementally compresses input and returns the new
stable output without flushing or recompressing earlier input. `finish` returns
the remaining suffix. Concatenating those bytes produces one protocol-compatible
channel, including Brotli's channel-version byte.

`BrotliLevel::compress_channel` is the one-shot API used by fixtures.
`BrotliCompressor` exposes raw stateless Brotli output.
Without the `std` feature, compression returns `BrotliUnavailable`.

## Synthetic incompressible-data benchmark

The `base-compression-benchmark` binary reports the actual number of bytes emitted by the
production streaming Brotli compressor after synthetic transaction-sized records are batched into
one channel. It includes the transaction-size model's starting profiles and can measure a new
profile without changing source code:

```sh
cargo run -p base-comp --features benchmark --bin base-compression-benchmark -- \
  --transactions-per-batch 128 --batches-per-channel 8 \
  --profile erc1155_transfer:512
```

The default report covers: Native ETH transfer, ERC-20 transfer (USDC), B20 transfer, Uniswap V3
/ aggregator swap, Uniswap V2 swap, x402 agentic payment, ERC-4337 smart-wallet `UserOp`, and
contract deployment.

The checked-in [incompressible-data results](benchmarks/incompressible-data.csv) are the rounded
whole-byte estimates for 1,024 synthetic pseudorandom transactions per channel (eight batches of
128). Fractional amortized stream overhead is intentionally omitted from this table.

Add `--incremental` to emit the new stable compressed bytes returned as each transaction is
appended. These rows sum exactly to the finished channel size. Because Brotli buffers input, a
row can be zero and the final row includes the stream trailer; it is a streaming byte allocation,
not a counterfactual recompression of a finalized prefix.

The CSV output distinguishes deterministic pseudorandom bytes (the conservative
incompressible-data case) from incrementing bytes (a deliberately compressible control). Each
input is wrapped in a deterministic, locally signed EIP-1559 envelope before compression; no RPC
endpoint or deployed contract is required. Reported bytes are channel payload only and exclude
blob framing and L1 calldata overhead.

Run the accompanying Criterion throughput benchmark with:

```sh
cargo bench -p base-comp --features benchmark --bench incompressible_data
```

Batch encoding, sizing, and framing belong to `base-batcher-encoder`; this crate
only transforms uncompressed channel bytes into their protocol compression
format.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
