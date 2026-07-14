# base-proof-tee-tdx-runtime

Runtime helpers for Intel TDX signer identity and quote collection.

The crate owns secp256k1 signer key generation inside the guest, derives the
uncompressed signer public key and Ethereum address, builds the
`TDREPORT.REPORTDATA` value expected by the TDX verifier, and collects quotes
through a narrow provider trait.

The production provider targets Linux TSM/configfs quote collection. Local tests
use a deterministic mock provider so CI does not require TDX hardware.

`TDREPORT.REPORTDATA` binds the signer key, the CI-derived OCI manifest digest,
registrar nonce, quote timestamp, L1 chain ID, and `TEEProverRegistry` address.
