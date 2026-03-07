# L2 Execution Engine

## EVM Changes

### `bn256Pairing` precompile input restriction

The `bn256Pairing` precompile execution has additional validation on its input.
The precompile reverts if its input is larger than `112687` bytes.
This is the input size that consumes approximately 20 M gas given the latest `bn256Pairing` gas schedule on L2.
