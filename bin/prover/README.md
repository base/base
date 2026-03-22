# `base-prover`

Prover binary supporting TEE and ZK proving backends.

## `nitro` — TEE (AWS Nitro Enclaves)

- **`nitro server`** — Runs the JSON-RPC server on the EC2 host, forwarding proving requests to the enclave over vsock.
- **`nitro enclave`** — Runs the proving process inside the Nitro Enclave, listening on vsock.
- **`nitro local`** *(feature-gated)* — Runs server and enclave in a single process for local development.

### Inspecting the enclave

**Remotely (from your local machine):**

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
the enclave image (EIF) is rebuilt. The `teeImageHash` used on-chain is
`keccak256(PCR0_raw_bytes)`.

## `zk` — ZK prover service

Runs the gRPC ZK prover server. Reads proof requests from a database outbox, dispatches them to a cluster backend, and stores artifacts in Redis, S3, or GCS.
