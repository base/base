# `base-comp`

Channel compression for Base.

## Overview

`CompressionStream::append` incrementally compresses input and returns the new
stable output without recompressing earlier input. With a flush interval, it
flushes the encoder each time that many input bytes have been appended, so the
output returned so far covers all but less than one interval of the input.
`finish` returns the remaining suffix. Concatenating those bytes produces one
protocol-compatible channel, including Brotli's channel-version byte.

`BrotliLevel::compress_channel` is the one-shot API used by fixtures.
`BrotliCompressor` exposes raw stateless Brotli output.
Without the `std` feature, compression returns `BrotliUnavailable`.

Batch encoding, sizing, and framing belong to `base-batcher-encoder`; this crate
only transforms uncompressed channel bytes into their protocol compression
format.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
