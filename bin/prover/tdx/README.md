# TDX Prover Binary

Worker binary for Intel TDX TEE proof backends.

The binary claims Intel TDX proof jobs from prover-service and serves only the
registrar-facing signer JSON-RPC methods plus `/healthz`.

The binary contains CLI glue only. TDX signer, quote, proof, and worker behavior
is implemented in `base-proof-tee-tdx-prover` and `base-proof-tee-tdx-runtime`.
