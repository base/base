# MEV provisioning binaries

`base-mev-suppression-provision` creates the suppression rollback anchor. `base-mev-t4e-provision` is the offline, no-network publication path for the production T4e simulation prerequisites. Neither binary signs transactions or owner attestations, submits data, enables runtime activation, funds an account, or starts/restarts a node.

## Build

```sh
cargo build -p base-suppression-provision-bin --features provision --bin base-mev-t4e-provision
```

The provisioning feature is not part of the node build and does not enable `arm-live-egress`.

## Initial T4e provisioning

1. Prepare the compile-pinned private directories:

   ```sh
   target/debug/base-mev-t4e-provision prepare
   ```

2. Create or idempotently reopen the R9 claim store:

   ```sh
   target/debug/base-mev-t4e-provision claim-store
   ```

   The command prints the 32-byte lowercase hexadecimal store identity. Preserve the existing store across reruns. The printed identity is an input to the deployment attestation and must match `r9_store_identity` in the install bundle.

3. **Owner-only, on the offline owner signer:** freeze the ordered source population, build the terminal settlement projection, and sign their exact canonical preimages. Build the G7, live-window, and Base deployment pairs, then sign the install bundle's outer `domain || bundle_content_hash` preimage. The canonical preparation and validation surfaces are `ProducerConformance::prepare_frozen_manifest`, `ProducerConformance::prepare_install_bundle`, `SignedPopulationManifestV1`, `SignedProjectionV1`, and `SignedInstallBundleV1`. Copy only the resulting signed canonical files to this host.

4. Make each staged signed file an owner-owned, single-link regular file with mode `0600`. Keep staged files outside the compile-pinned publication directories, whose inventories are closed.

5. Validate and publish all three signed artifacts:

   ```sh
   target/debug/base-mev-t4e-provision publish-population /secure-staging/manifest.bin
   target/debug/base-mev-t4e-provision publish-projection /secure-staging/projection.bin
   target/debug/base-mev-t4e-provision publish-install-bundle /secure-staging/authority.bundle
   ```

   Publication rejects malformed or non-canonical bytes, invalid owner signatures, unsafe file metadata, mixed bundle generations, stale temporary files, unknown directory entries, and conflicting immutable population bytes. Writes are synchronized and atomically published at the compile-pinned paths.

6. Node startup with the separately controlled `arm-sim` runtime activation performs the real-chain finality, canonical-hash, population-equality, completeness, and rollback checks. Only after those checks does `NodeLocalSettledLossAuthority::prepare_complete` create or advance `settled-loss-v1/accepted-head`. Do not create, delete, copy, or rewind `accepted-head` manually.

A ready installation therefore consists of the owner-signed population, owner-signed projection, owner-signed install bundle, identity-bound claim store, and the node-validated monotonic accepted head. Authenticated settled loss of zero is valid; a missing projection or accepted head is not treated as zero.

## Successor publications

Generate and sign successor projection or bundle bytes only in the offline owner process. Publish them with the same commands. Never replace the R9 claim store to obtain a new identity without also producing a newly owner-approved deployment attestation and install bundle. Never rewind or remove the accepted head; rollback detection is intentionally fail-closed.
