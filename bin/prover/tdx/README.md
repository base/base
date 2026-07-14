# TDX Prover Binary

Worker binary for Intel TDX TEE proof backends.

The binary claims Intel TDX proof jobs from prover-service and serves only the
registrar-facing signer JSON-RPC methods plus `/healthz`.

The binary contains CLI glue only. TDX signer, attestation-token, proof, and worker behavior
is implemented in `base-proof-tee-tdx-prover` and `base-proof-tee-tdx-runtime`.

Production runs inside Google Confidential Space. The launcher supplies
short-lived Google Cloud Attestation PKI tokens through
`/run/container_launcher/teeserver.sock`; the prover reads the OCI image digest
from those tokens when signing proof journals.

`TEE_TDX_IMAGE_HASH` is the 32-byte OCI manifest digest attested by
Confidential Space and configured on `AggregateVerifier`. It is not supplied to
the production prover process.
