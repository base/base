/// NSM (Nitro Secure Module) session management and random number generation.

#[cfg(target_os = "linux")]
use aws_nitro_enclaves_nsm_api::api::{Request, Response};
#[cfg(target_os = "linux")]
use aws_nitro_enclaves_nsm_api::driver::{nsm_exit, nsm_init, nsm_process_request};
use rand_08::{CryptoRng, RngCore};
use tracing::warn;

use crate::error::{NitroError, NsmError};

// ---------------------------------------------------------------------------
// NsmSession
// ---------------------------------------------------------------------------

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
    #[cfg(target_os = "linux")]
    pub fn open() -> Result<Option<Self>, NitroError> {
        let fd = nsm_init();
        if fd < 0 {
            warn!("failed to open Nitro Secure Module session, running in local mode");
            Ok(None)
        } else {
            Ok(Some(Self { fd }))
        }
    }

    /// Attempt to open an NSM session.
    ///
    /// On non-Linux platforms, always returns `None` (local mode).
    #[cfg(not(target_os = "linux"))]
    pub fn open() -> Result<Option<Self>, NitroError> {
        warn!("NSM not available on this platform, running in local mode");
        Ok(None)
    }

    /// Describe PCR0 (the enclave image measurement).
    ///
    /// Returns the PCR0 value as a byte vector.
    #[cfg(target_os = "linux")]
    pub fn describe_pcr0(&self) -> Result<Vec<u8>, NitroError> {
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
    pub fn describe_pcr0(&self) -> Result<Vec<u8>, NitroError> {
        Err(NsmError::DescribePcr("NSM not available".to_string()).into())
    }

    /// Get an attestation document with the given public key.
    ///
    /// The public key is included in the attestation document.
    #[cfg(target_os = "linux")]
    pub fn get_attestation(&self, public_key: Vec<u8>) -> Result<Vec<u8>, NitroError> {
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
    pub fn get_attestation(&self, _public_key: Vec<u8>) -> Result<Vec<u8>, NitroError> {
        Err(NsmError::Attestation("NSM not available".to_string()).into())
    }

    /// Get random bytes from the NSM device.
    #[cfg(target_os = "linux")]
    pub fn get_random(&self, count: usize) -> Result<Vec<u8>, NitroError> {
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
    pub fn get_random(&self, _count: usize) -> Result<Vec<u8>, NitroError> {
        Err(NsmError::Random("NSM not available".to_string()).into())
    }
}

#[cfg(target_os = "linux")]
impl Drop for NsmSession {
    fn drop(&mut self) {
        nsm_exit(self.fd);
    }
}

// ---------------------------------------------------------------------------
// NsmRng
// ---------------------------------------------------------------------------

/// A cryptographically secure random number generator backed by NSM.
///
/// On Linux, uses the NSM hardware RNG.
/// On non-Linux platforms, falls back to the OS RNG.
#[derive(Debug)]
pub struct NsmRng {
    #[cfg(target_os = "linux")]
    fd: i32,
    #[cfg(not(target_os = "linux"))]
    inner: rand_08::rngs::ThreadRng,
}

impl NsmRng {
    /// Create a new NSM-backed RNG.
    ///
    /// On Linux, opens an NSM session for random number generation.
    /// On other platforms, falls back to the thread RNG.
    #[cfg(target_os = "linux")]
    pub fn new() -> Option<Self> {
        let fd = nsm_init();
        if fd < 0 { None } else { Some(Self { fd }) }
    }

    /// Create a new NSM-backed RNG.
    ///
    /// On non-Linux platforms, always returns a fallback RNG.
    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Option<Self> {
        Some(Self { inner: rand_08::thread_rng() })
    }

    /// Create a fallback RNG using the OS random source.
    #[cfg(not(target_os = "linux"))]
    pub fn fallback() -> Self {
        Self { inner: rand_08::thread_rng() }
    }

    /// Create a fallback RNG using the OS random source.
    ///
    /// On Linux, uses fd = -1 as a sentinel value. The `fill_bytes`
    /// implementation checks for this and falls back to `OsRng`.
    #[cfg(target_os = "linux")]
    pub const fn fallback() -> Self {
        Self { fd: -1 }
    }
}

impl Default for NsmRng {
    fn default() -> Self {
        Self::new().unwrap_or_else(Self::fallback)
    }
}

#[cfg(target_os = "linux")]
impl Drop for NsmRng {
    fn drop(&mut self) {
        if self.fd >= 0 {
            nsm_exit(self.fd);
        }
    }
}

impl RngCore for NsmRng {
    #[cfg(target_os = "linux")]
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    #[cfg(not(target_os = "linux"))]
    fn next_u32(&mut self) -> u32 {
        self.inner.next_u32()
    }

    #[cfg(target_os = "linux")]
    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    #[cfg(not(target_os = "linux"))]
    fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    #[cfg(target_os = "linux")]
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        if self.fd < 0 {
            rand_08::rngs::OsRng.fill_bytes(dest);
            return;
        }

        let mut filled = 0;
        while filled < dest.len() {
            let request = Request::GetRandom;
            let response = nsm_process_request(self.fd, request);

            match response {
                Response::GetRandom { random } => {
                    let remaining = dest.len() - filled;
                    let to_copy = remaining.min(random.len());
                    dest[filled..filled + to_copy].copy_from_slice(&random[..to_copy]);
                    filled += to_copy;
                }
                _ => {
                    rand_08::rngs::OsRng.fill_bytes(&mut dest[filled..]);
                    return;
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.inner.fill_bytes(dest);
    }

    #[cfg(target_os = "linux")]
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_08::Error> {
        self.fill_bytes(dest);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_08::Error> {
        self.inner.try_fill_bytes(dest)
    }
}

impl CryptoRng for NsmRng {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_open_session_on_non_linux() {
        let session = NsmSession::open().unwrap();
        assert!(session.is_none());
    }

    #[test]
    fn test_nsm_rng_fill_bytes() {
        let mut rng = NsmRng::default();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
    }

    #[test]
    fn test_nsm_rng_next_u64() {
        let mut rng = NsmRng::default();
        let _ = rng.next_u64();
    }
}
