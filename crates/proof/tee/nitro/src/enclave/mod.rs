/// Enclave runtime: vsock listener, server, crypto, NSM, and attestation.
use std::io::{Read, Write};

use alloy_primitives::{Address, B256};

mod attestation;
pub use attestation::{
    AttestationDocument, AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256,
    VerificationResult, VerifyOptions, extract_public_key, get_default_ca_root,
    verify_attestation, verify_attestation_with_options, verify_attestation_with_pcr0,
    verify_attestation_with_pcr0_and_options,
};

mod crypto;
pub use crypto::{Ecdsa, SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, Signing};

mod nsm;
pub use nsm::{NsmRng, NsmSession};

mod server;
pub use server::Server;

/// Enclave runtime configuration.
pub struct EnclaveConfig {
    /// Vsock port to listen on.
    pub vsock_port: u32,
    /// The proposer address (set at deploy time).
    pub proposer: Address,
    /// Per-chain configuration hash.
    pub config_hash: B256,
    /// Expected PCR0 measurement. Verified against NSM at startup.
    pub tee_image_hash: B256,
}

// ---------------------------------------------------------------------------
// Frame helpers (same format as base-proof-transport/src/vsock.rs)
// ---------------------------------------------------------------------------

fn write_frame<T: serde::Serialize>(writer: &mut impl Write, value: &T) -> eyre::Result<()> {
    let payload = bincode::serde::encode_to_vec(value, bincode::config::standard())?;

    let len = u32::try_from(payload.len()).map_err(|_| eyre::eyre!("payload exceeds u32::MAX"))?;

    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> eyre::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;

    let (value, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())?;
    Ok(value)
}

// ---------------------------------------------------------------------------
// Vsock listener (Linux only)
// ---------------------------------------------------------------------------

/// Run the enclave process: listen on vsock, prove blocks, return results.
#[cfg(target_os = "linux")]
pub async fn run(config: EnclaveConfig) -> eyre::Result<()> {
    use std::time::Duration;

    use base_proof_primitives::{ProofBundle, ProofResult};
    use tracing::{debug, info};
    use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

    const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    let server = Server::new(&config)?;
    info!(address = %server.signer_address(), "enclave initialized");

    let listener = VsockListener::bind(&VsockAddr::new(VMADDR_CID_ANY, config.vsock_port))?;
    info!(port = config.vsock_port, "listening on vsock");

    loop {
        let (mut stream, peer) = listener.accept()?;
        debug!(cid = peer.cid(), port = peer.port(), "accepted connection");

        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        let bundle: ProofBundle = read_frame(&mut stream)?;
        let result: ProofResult = server.prove(bundle).await?;
        write_frame(&mut stream, &result)?;
    }
}
