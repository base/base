# `base-prover-nitro-host`

TEE prover host worker for AWS Nitro Enclaves.

## Subcommands

- **`server`** — Claims Nitro TEE jobs from `PROVER_SERVICE_ENDPOINT` and forwards them to the enclave over vsock. On Linux, it also exposes the registrar-facing signer JSON-RPC API.
- **`local`** *(feature-gated)* — Claims Nitro TEE jobs using in-process local enclave instances for local development.

## Worker Mode

Worker mode is the only supported runtime mode:

```bash
cargo build --package base-prover-nitro-host
```

It requires `PROVER_SERVICE_ENDPOINT` and claims AWS Nitro TEE jobs through the
prover-service worker API.

For local worker development, enable the `local` feature and use `local`:

```bash
cargo run --package base-prover-nitro-host --features local -- local \
  --prover-service-endpoint "$PROVER_SERVICE_ENDPOINT" \
  --l1-eth-url "$L1_ETH_URL" \
  --l2-eth-url "$L2_ETH_URL" \
  --l1-beacon-url "$L1_BEACON_URL" \
  --l2-chain-id "$L2_CHAIN_ID"
```

The `just tee nitro-local-worker` recipe wraps the same command.

## Inspecting the enclave

**Remotely (from your local machine):**

```bash
# Get the enclave signer's Ethereum address
just tee signer-address https://<PROVER_RPC_URL>

# Get PCR0 and teeImageHash (requires: pip3 install cbor2)
just tee remote-pcr0 https://<PROVER_RPC_URL>
```

<details>
<summary>Manual steps (what the recipes do under the hood)</summary>

```bash
# Get the enclave's signer public key
cast rpc enclave_signerPublicKey -r https://<PROVER_RPC_URL>

# Derive the Ethereum address from the public key
PUB_KEY_HEX=$(python3 -c 'data=[<PASTE_BYTE_ARRAY>]; print("0x" + bytes(data[1:]).hex())')
HASH=$(cast keccak $PUB_KEY_HEX)
cast to-check-sum-address "0x${HASH: -40}"

# Get the PCR0 from the attestation document
pip3 install cbor2
cast rpc enclave_signerAttestation -r https://<PROVER_RPC_URL>
# Then parse the CBOR attestation:
python3 -c "import cbor2; data=bytes([<PASTE_BYTE_ARRAY>]); _, _, payload, _ = cbor2.loads(data); doc = cbor2.loads(payload); print('PCR0:', doc['pcrs'][0].hex())"

# Compute the teeImageHash (keccak of raw PCR0 bytes)
cast keccak 0x<PCR0_HEX>
```

</details>

**Via SSH (on the EC2 host):**

```bash
# The instance IP can be found in Datadog by clicking any Prover log entry
# and checking the `data.hostname` field.
ssh root@<INSTANCE_IP>

# List running containers to find the prover
docker ps --format "{{.ID}} {{.Image}} {{.Command}}"

# Get enclave measurements including PCR0
docker exec <PROVER_CONTAINER_ID> /app/nitro-cli describe-enclaves
```

The `PCR0` in the output is the enclave image measurement. It only changes when
the enclave image (EIF) is rebuilt. The `teeImageHash` used onchain is
`keccak256(PCR0_raw_bytes)`.
