# Prototype caveats

## SP1 ZK proving removed

The SP1 prover was removed because of dependency version incompatibilities. Our
execution stack uses Alloy 2.4.1, while SP1 SDK 6.4.0's network signing dependencies
require Alloy 1.x. This pulled both `alloy-rpc-types-eth` 1.8.3 and 2.4.1 into the
workspace. Updating to SP1 6.6.0 would still require Alloy 1.x.

This prototype therefore has no SP1 prover, guest programs, ELF builds, network or
cluster proving backends, or SP1 receipt decoding/submission. The associated ZK
benchmark and dispute tools have also been removed. New compressed and SNARK/PLONK
proof requests are rejected by the prover service. The challenger's ZK fallback
cannot produce or submit proofs; deployments that require ZK challenges or
nullification are unsupported.

Nitro/TEE proving and normal node execution remain. Legacy ZK protocol types and
contract interfaces are retained for compatibility, but do not imply working ZK
proving or verification coverage. Running the devnet does not validate ZK security.

Restoring SP1 would require a compatible upstream release or an SP1 patch that
upgrades its Alloy dependencies, followed by rebuilding the guest programs and
validating real proof generation and on-chain verification. Removing SP1 addresses
this Alloy 1.x dependency chain; it does not guarantee that every dependency in the
workspace has a single version.
