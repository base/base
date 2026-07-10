# TDX Image Hash Tool

Queries a TDX prover endpoint and prints the contract-compatible image hash
that operators should deploy as `AggregateVerifier.TEE_IMAGE_HASH`.

For TDX, `AggregateVerifier.TEE_IMAGE_HASH` must equal the TDX verifier journal
`imageHash`, computed as:

```text
keccak256(MRTD || RTMR0 || RTMR1 || RTMR2 || RTMR3)
```

It must not be configured to the raw MRTD value or to `keccak256(MRTD)` alone.

Example:

```sh
cargo run -p base-proof-tee-tdx-image-hash -- \
  --endpoint http://127.0.0.1:7310
```

To verify the quote locally before printing an operator-ready result, use the
same Intel PCS collateral settings as the registrar:

```sh
cargo run -p base-proof-tee-tdx-image-hash -- \
  --endpoint http://tdx-prover.example:7310 \
  --verify-quote \
  --pcs-tdx-base-url https://api.trustedservices.intel.com/tdx/certification/v4/ \
  --trusted-root-ca-hash 0xa1acc73eb45794fa1734f14d882e91925b6006f79d3bb2460df9d01b333d7009
```

After registration, pass the registry address and L1 RPC URL to confirm that
the registered `signerImageHash` equals the printed `imageHash` and that
`isValidSigner` reflects whether the current `AggregateVerifier.TEE_IMAGE_HASH`
matches it:

```sh
cargo run -p base-proof-tee-tdx-image-hash -- \
  --endpoint http://tdx-prover.example:7310 \
  --l1-rpc-url http://127.0.0.1:8545 \
  --registry-address 0x0000000000000000000000000000000000000000
```
