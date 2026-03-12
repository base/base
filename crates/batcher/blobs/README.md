# `base-blobs`

EIP-4844 blob encoding and decoding for the Base batcher.

`BlobEncoder` packs raw bytes into the BLS field-element-compatible wire
format used by the Base derivation pipeline. Each 32-byte field element
carries 31 payload bytes in its lower bytes; the high byte of every group of
four field elements carries six-bit chunks that reassemble into three
additional payload bytes. A version byte and 3-byte big-endian length are
written into field element 0. A single blob holds up to 130,044 bytes of
payload data across 1,024 encoding rounds.

`BlobDecoder` inverts the encoding: it validates the version, reads the
declared length, extracts payload bytes from each field element, reassembles
the packed high-byte chunks, and verifies that all trailing bytes are zero.

This crate does not perform KZG commitment, proof generation, or blob
submission — those concerns belong in higher-level crates.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
