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

3. Prepare each signing request on this host, transfer its `preimage.bin` to the
   offline owner signer, verify the generated `digest.hex`, and attach the returned
   signature:

   ```sh
   target/debug/base-mev-t4e-provision prepare-population \
     /secure-staging/export.json /secure-staging/population-request
   # Offline owner signs population-request/preimage.bin.
   target/debug/base-mev-t4e-provision attach-population \
     /secure-staging/population-request /secure-staging/population.sig \
     /secure-staging/manifest.bin

   target/debug/base-mev-t4e-provision prepare-projection \
     /secure-staging/export.json /secure-staging/manifest.bin \
     /secure-staging/projection-fields.json /secure-staging/projection-request
   # Offline owner signs projection-request/preimage.bin.
   target/debug/base-mev-t4e-provision attach-projection \
     /secure-staging/projection-request /secure-staging/projection.sig \
     /secure-staging/projection.bin

   target/debug/base-mev-t4e-provision prepare-install-bundle \
     /secure-staging/install-fields.json /secure-staging/install-request
   # Offline owner signs install-request/preimage.bin.
   target/debug/base-mev-t4e-provision attach-install-bundle \
     /secure-staging/install-request /secure-staging/install.sig \
     /secure-staging/authority.bundle
   ```

   Every prepare command creates a new mode-`0700` request directory containing
   mode-`0600` `unsigned.bin`, `preimage.bin`, and `digest.hex`. `digest.hex` is the
   lowercase `0x`-prefixed EIP-191 digest of the exact bytes in `preimage.bin`, with
   one trailing LF. Request directories and files are create-new: an existing path,
   partial prior request, malformed export or fields file, or population-membership
   mismatch fails closed and must be investigated rather than overwritten.

4. A signature file is exactly one of:

   * 65 raw signature bytes; or
   * lowercase ASCII `0x` followed by exactly 130 hexadecimal digits, either with no
     terminator or with exactly one trailing LF.

   CRLF, two LFs, embedded newlines, spaces, uppercase hex, empty or oversized files,
   symlinks, FIFOs, and other non-regular inputs are rejected. Reads use a bounded,
   no-follow, nonblocking opened handle. Attach validates the prepared body/preimage
   relationship and creates the signed output with mode `0600`; it never overwrites an
   existing output. A failed attach or prepare publishes nothing.

5. Keep each staged signed file owner-owned, single-link, regular, and mode `0600`.
   Keep staged files outside the compile-pinned publication directories, whose
   inventories are closed.

6. Validate and publish all three signed artifacts:

   ```sh
   target/debug/base-mev-t4e-provision publish-population /secure-staging/manifest.bin
   target/debug/base-mev-t4e-provision publish-projection /secure-staging/projection.bin
   target/debug/base-mev-t4e-provision publish-install-bundle /secure-staging/authority.bundle
   ```

   Publication rejects malformed or non-canonical bytes, invalid owner signatures,
   unsafe file metadata, mixed bundle generations, stale temporary files, unknown
   directory entries, and conflicting immutable population bytes. Writes are
   synchronized and atomically published at the compile-pinned paths.

7. Node startup with the separately controlled `arm-sim` runtime activation performs the real-chain finality, canonical-hash, population-equality, completeness, and rollback checks. Only after those checks does `NodeLocalSettledLossAuthority::prepare_complete` create or advance `settled-loss-v1/accepted-head`. Do not create, delete, copy, or rewind `accepted-head` manually.

A ready installation therefore consists of the owner-signed population, owner-signed projection, owner-signed install bundle, identity-bound claim store, and the node-validated monotonic accepted head. Authenticated settled loss of zero is valid; a missing projection or accepted head is not treated as zero.

## Successor publications

Generate and sign successor projection or bundle bytes only in the offline owner process. Publish them with the same commands. Never replace the R9 claim store to obtain a new identity without also producing a newly owner-approved deployment attestation and install bundle. Never rewind or remove the accepted head; rollback detection is intentionally fail-closed.
