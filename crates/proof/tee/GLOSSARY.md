# Nitro TEE prover

Terms for the AWS Nitro Enclaves trusted execution environment (TEE) prover: the host process that serves proving and registration requests, and the enclave that re-executes blocks and signs the result.

## Language

**Enclave link**:
The request/response hop between the host and one enclave, covering the wire messages, their checks, timeouts, and failure kinds, whether the enclave runs behind vsock or in-process.
_Avoid_: transport, vsock transport, enclave channel

**Frame**:
The byte-level encoding of one Enclave link message: a big-endian `u64` length followed by the bincode payload.
_Avoid_: packet, envelope

**Link failure**:
An Enclave link request that did not get a valid answer from the enclave: connect or timeout failure, a bad Frame, a reply of the wrong type, or a malformed signer key.
_Avoid_: transport error

**Enclave rejection**:
An answer in which the enclave refused or failed the request: invalid request, proof failure, or attestation failure.
_Avoid_: remote error, enclave error
