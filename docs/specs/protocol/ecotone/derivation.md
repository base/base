# Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Ecotone: Blob Retrieval](#ecotone-blob-retrieval)
- [Blob Encoding](#blob-encoding)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Ecotone: Blob Retrieval

With the Ecotone upgrade the retrieval stage is extended to support an additional DA source:
[EIP-4844] blobs. After the Ecotone upgrade we modify the iteration over batcher transactions to
treat transactions of transaction-type == `0x03` (`BLOB_TX_TYPE`) differently. If the batcher
transaction is a blob transaction, then its calldata MUST be ignored should it be present. Instead:

- For each blob hash in `blob_versioned_hashes`, retrieve the blob that matches it. A blob may be
  retrieved from any of a number different sources. Retrieval from a local beacon-node, through
  the `/eth/v1/beacon/blob_sidecars/` endpoint, with `indices` filter to skip unrelated blobs, is
  recommended. For each retrieved blob:
  - The blob SHOULD (MUST, if the source is untrusted) be cryptographically verified against its
    versioned hash.
  - If the blob has a [valid encoding](#blob-encoding), decode it into its continuous byte-string
    and pass that on to the next phase. Otherwise the blob is ignored.

Note that batcher transactions of type blob must be processed in the same loop as other batcher
transactions to preserve the invariant that batches are always processed in the order they appear
in the block. We ignore calldata in blob transactions so that it may be used in the future for
batch metadata or other purposes.

## Blob Encoding

Each blob in a [EIP-4844] transaction really consists of `FIELD_ELEMENTS_PER_BLOB = 4096` field elements.

Each field element is a number in a prime field of
`BLS_MODULUS = 52435875175126190479447740508185965837690552500527637822603658699938581184513`.
This number does not represent a full `uint256`: `math.log2(BLS_MODULUS) = 254.8570894...`

The [L1 consensus-specs](https://github.com/ethereum/consensus-specs/blob/dev/specs/deneb/polynomial-commitments.md)
describe the encoding of this polynomial.
The field elements are encoded as big-endian integers (`KZG_ENDIANNESS = big`).

To save computational overhead, only `254` bits per field element are used for rollup data.

For efficient data encoding, `254` bits (equivalent to `31.75` bytes) are utilized.
`4` elements combine to effectively use `127` bytes.

`127` bytes of application-layer rollup data is encoded at a time, into 4 adjacent field elements of the blob:

```python
# read(N): read the next N bytes from the application-layer rollup-data. The next read starts where the last stopped.
# write(V): append V (one or more bytes) to the raw blob.
bytes tailA = read(31)
byte x = read(1)
byte A = x & 0b0011_1111
write(A)
write(tailA)

bytes tailB = read(31)
byte y = read(1)
byte B = (y & 0b0000_1111) | (x & 0b1100_0000) >> 2)
write(B)
write(tailB)

bytes tailC = read(31)
byte z = read(1)
byte C = z & 0b0011_1111
write(C)
write(tailC)

bytes tailD = read(31)
byte D = ((z & 0b1100_0000) >> 2) | ((y & 0b1111_0000) >> 4)
write(D)
write(tailD)
```

Each written field element looks like this:

- Starts with one of the prepared 6-bit left-padded byte values, to keep the field element within valid range.
- Followed by 31 bytes of application-layer data, to fill the low 31 bytes of the field element.

The written output should look like this:

```text
<----- element 0 -----><----- element 1 -----><----- element 2 -----><----- element 3 ----->
| byte A |  tailA...  || byte B |  tailB...  || byte C |  tailC...  || byte D |  tailD...  |
```

The above is repeated 1024 times, to fill all `4096` elements,
with a total of `(4 * 31 + 3) * 1024 = 130048` bytes of data.

When decoding a blob, the top-most two bits of each field-element must be 0,
to make the encoding/decoding bijective.

The first byte of rollup-data (second byte in first field element) is used as a version-byte.

In version `0`, the next 3 bytes of data are used to encode the length of the rollup-data, as big-endian `uint24`.
Any trailing data, past the length delimiter, must be 0, to keep the encoding/decoding bijective.
If the length is larger than `130048 - 4`, the blob is invalid.

If any of the encoding is invalid, the blob as a whole must be ignored.

[EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
