# base-proof-tee-tdx-verifier

Pure Intel TDX quote verification logic for TDX signer registration.

The crate accepts all quote bytes, quote collection timestamp metadata,
collateral, signing chains, revocation evidence, trust anchors, policy inputs,
signer binding inputs, and verification time through an explicit
`TdxVerifierInput`. It does not read from the filesystem, perform network
requests, or depend on registrar or transaction manager crates, so the same
logic can be compiled into a ZK guest and tested natively.

`TDREPORT.REPORTDATA` must bind the expected signer key hash in its first 32
bytes. Its last 32 bytes are
`keccak256("base-tdx-workload-v2" || workload_digest || registrar_nonce || quote_timestamp_millis_le || chain_id_le || registry_address)`.
The verifier rejects quotes from debug TDs and emits the signed workload digest
as `TDXVerifierJournal.imageHash`.
