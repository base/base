# base-proof-tee-tdx-verifier

Pure Google Confidential Space token verification logic for TDX signer registration.

The crate accepts a Google Cloud Attestation PKI token, its embedded X.509
chain, trust-anchor hash, workload policy, signer binding inputs, and
verification time through an explicit `TdxVerifierInput`. It does not read from
the filesystem, perform network requests, or depend on registrar or transaction
manager crates, so the same logic can be compiled into a ZK guest and tested
natively.

The verifier requires `CONFIDENTIAL_SPACE`, `GCP_INTEL_TDX`, Secure Boot, a
production non-debug launcher, no command or environment overrides, the
expected audience, and a signer-bound registrar nonce. It emits the attested OCI
workload image digest as `TDXVerifierJournal.imageHash`.
