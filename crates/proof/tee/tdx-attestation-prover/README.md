# TDX Attestation Prover

Transforms explicit Intel TDX quote verification input into signer registration
proof material for `TEEProverRegistry.registerTDXSigner`.

The crate exposes a native direct path for local development and tests without
TDX hardware. Production RISC Zero proving through the Boundless marketplace is
available behind the `prove` feature.
