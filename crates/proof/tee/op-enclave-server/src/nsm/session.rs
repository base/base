//! NSM (Nitro Secure Module) session management.
//!
//! This module provides session management for AWS Nitro Enclave's NSM device.
//! On non-Linux platforms, NSM is unavailable and operations return `None`.
//!
//! # Ephemeral Session Pattern
//!
//! NSM sessions are intentionally short-lived and opened fresh for each operation
//! (attestation, PCR read, etc.) rather than maintaining a persistent session.
//! This design follows several important principles:
//!
//! - **Resource hygiene**: The NSM file descriptor is a limited system resource.
//!   Opening it only when needed and closing it immediately after ensures we don't
//!   hold onto system resources unnecessarily.
//!
//! - **Stateless operations**: Each NSM request is independent. The device doesn't
//!   maintain state between requests, so there's no benefit to keeping a session open.
//!
//! - **Error isolation**: If an NSM operation fails, a fresh session ensures the next
//!   operation starts with a clean slate rather than inheriting potentially corrupted state.
//!
//! - **Go implementation parity**: The Go enclave implementation (`op-enclave/enclave/server.go`)
//!   follows the same pattern, opening sessions per-operation for consistency.

use crate::error::{NsmError, ServerError};

#[cfg(target_os = "linux")]
use aws_nitro_enclaves_nsm_api::api::{Request, Response};
#[cfg(target_os = "linux")]
use aws_nitro_enclaves_nsm_api::driver::{nsm_exit, nsm_init, nsm_process_request};

/// A session with the Nitro Secure Module.
///
/// On Linux, this wraps an NSM file descriptor.
/// On other platforms, this is a stub that always fails.
#[derive(Debug)]
pub struct NsmSession {
    #[cfg(target_os = "linux")]
    fd: i32,
    #[cfg(not(target_os = "linux"))]
    _private: (),
}

impl NsmSession {
    /// Attempt to open an NSM session.
    ///
    /// Returns `Ok(Some(session))` if NSM is available.
    /// Returns `Ok(None)` if NSM is unavailable (non-Linux or device not present).
    /// Returns `Err` if there's an unexpected error.
    #[cfg(target_os = "linux")]
    pub fn open() -> Result<Option<Self>, ServerError> {
        let fd = nsm_init();
        if fd < 0 {
            tracing::warn!("failed to open Nitro Secure Module session, running in local mode");
            Ok(None)
        } else {
            Ok(Some(Self { fd }))
        }
    }

    /// Attempt to open an NSM session.
    ///
    /// On non-Linux platforms, always returns `None` (local mode).
    #[cfg(not(target_os = "linux"))]
    pub fn open() -> Result<Option<Self>, ServerError> {
        tracing::warn!("NSM not available on this platform, running in local mode");
        Ok(None)
    }

    /// Describe PCR0 (the enclave image measurement).
    ///
    /// Returns the PCR0 value as a byte vector.
    #[cfg(target_os = "linux")]
    pub fn describe_pcr0(&self) -> Result<Vec<u8>, ServerError> {
        let request = Request::DescribePCR { index: 0 };
        let response = nsm_process_request(self.fd, request);

        match response {
            Response::DescribePCR { lock: _, data } => {
                if data.is_empty() {
                    Err(NsmError::NoPcrData.into())
                } else {
                    Ok(data)
                }
            }
            Response::Error(err) => Err(NsmError::DescribePcr(format!("{err:?}")).into()),
            _ => Err(NsmError::DescribePcr("unexpected response".to_string()).into()),
        }
    }

    /// Describe PCR0 - stub for non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    pub fn describe_pcr0(&self) -> Result<Vec<u8>, ServerError> {
        Err(NsmError::DescribePcr("NSM not available".to_string()).into())
    }

    /// Get an attestation document with the given public key.
    ///
    /// The public key is included in the attestation document.
    #[cfg(target_os = "linux")]
    pub fn get_attestation(&self, public_key: Vec<u8>) -> Result<Vec<u8>, ServerError> {
        let request = Request::Attestation {
            user_data: None,
            nonce: None,
            public_key: Some(public_key.into()),
        };
        let response = nsm_process_request(self.fd, request);

        match response {
            Response::Attestation { document } => Ok(document),
            Response::Error(err) => Err(NsmError::Attestation(format!("{err:?}")).into()),
            _ => Err(NsmError::NoAttestation.into()),
        }
    }

    /// Get attestation - stub for non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    pub fn get_attestation(&self, _public_key: Vec<u8>) -> Result<Vec<u8>, ServerError> {
        Err(NsmError::Attestation("NSM not available".to_string()).into())
    }

    /// Get random bytes from the NSM device.
    #[cfg(target_os = "linux")]
    pub fn get_random(&self, count: usize) -> Result<Vec<u8>, ServerError> {
        let mut result = Vec::with_capacity(count);

        while result.len() < count {
            let request = Request::GetRandom;
            let response = nsm_process_request(self.fd, request);

            match response {
                Response::GetRandom { random } => {
                    let remaining = count - result.len();
                    let to_copy = remaining.min(random.len());
                    result.extend_from_slice(&random[..to_copy]);
                }
                Response::Error(err) => {
                    return Err(NsmError::Random(format!("{err:?}")).into());
                }
                _ => {
                    return Err(NsmError::Random("unexpected response".to_string()).into());
                }
            }
        }

        Ok(result)
    }

    /// Get random bytes - stub for non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    pub fn get_random(&self, _count: usize) -> Result<Vec<u8>, ServerError> {
        Err(NsmError::Random("NSM not available".to_string()).into())
    }
}

#[cfg(target_os = "linux")]
impl Drop for NsmSession {
    fn drop(&mut self) {
        nsm_exit(self.fd);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_open_session_on_non_linux() {
        // On non-Linux, this should return None (local mode)
        #[cfg(not(target_os = "linux"))]
        {
            use super::*;
            let session = NsmSession::open().unwrap();
            assert!(session.is_none());
        }
    }
}
