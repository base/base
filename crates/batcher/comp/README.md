# `base-comp`

Channel compression for Base.

## Overview

`CompressionStream::append` incrementally compresses input and returns the new
stable output without flushing or recompressing earlier input. `finish` returns
the remaining suffix. Concatenating those bytes produces one protocol-compatible
channel. Brotli output includes its channel-version byte; zlib is self-identifying.

`CompressionAlgo::compress_channel` remains the one-shot API used by `no_std`
consumers and fixtures. `BrotliCompressor` exposes raw stateless Brotli output.

Batch encoding, sizing, and framing belong to `base-batcher-encoder`; this crate
only transforms uncompressed channel bytes into their protocol compression
format.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
