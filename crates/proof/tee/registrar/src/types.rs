use std::net::IpAddr;

use alloy_primitives::{Address, B256, Bytes};

/// A prover instance discovered from the infrastructure layer.
#[derive(Debug, Clone)]
pub struct ProverInstance {
    /// AWS EC2 instance ID.
    pub instance_id: String,
    /// Private IP address of the instance within the VPC.
    pub private_ip: IpAddr,
    /// Current health status reported by the ALB target group.
    pub health_status: InstanceHealthStatus,
}

/// Health status of a prover instance in the ALB target group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceHealthStatus {
    /// Health checks are in progress; instance is not yet receiving traffic.
    Initial,
    /// Instance is passing health checks and receiving proposal traffic.
    Healthy,
    /// Instance is failing health checks.
    Unhealthy,
    /// Instance is being decommissioned.
    Draining,
}

impl InstanceHealthStatus {
    /// Returns `true` if the instance should be registered (initial or healthy).
    pub const fn should_register(&self) -> bool {
        matches!(self, Self::Initial | Self::Healthy)
    }
}

/// Response from polling a prover's `enclave_signerAttestation` endpoint.
#[derive(Debug, Clone)]
pub struct AttestationResponse {
    /// Raw Nitro attestation document (`COSE_Sign1` bytes).
    pub attestation_bytes: Bytes,
    /// Ethereum address derived from the attestation's embedded public key.
    pub signer_address: Address,
}

/// ZK proof ready for on-chain registration via `TEEProverRegistry.registerSigner`.
#[derive(Debug, Clone)]
pub struct AttestationProof {
    /// ABI-encoded `VerifierJournal` output from the Boundless / Automata SDK.
    pub output: Bytes,
    /// ZK proof bytes for submission alongside `output`.
    pub proof_bytes: Bytes,
}

/// A signer currently registered on-chain via `TEEProverRegistry`.
#[derive(Debug, Clone)]
pub struct RegisteredSigner {
    /// The signer's Ethereum address.
    pub address: Address,
    /// The `keccak256(PCR0)` measurement hash the signer was registered under.
    pub pcr0: B256,
}
