//! NSM-based random number generator.
//!
//! This module provides an RNG that uses the NSM device for random bytes.

use rand::{CryptoRng, RngCore};

#[cfg(target_os = "linux")]
use aws_nitro_enclaves_nsm_api::api::{Request, Response};
#[cfg(target_os = "linux")]
use aws_nitro_enclaves_nsm_api::driver::{nsm_exit, nsm_init, nsm_process_request};

/// A cryptographically secure random number generator backed by NSM.
///
/// This RNG uses the Nitro Secure Module's hardware random number generator.
/// On non-Linux platforms, this falls back to the OS RNG.
#[derive(Debug)]
pub struct NsmRng {
    #[cfg(target_os = "linux")]
    fd: i32,
    #[cfg(not(target_os = "linux"))]
    inner: rand::rngs::ThreadRng,
}

impl NsmRng {
    /// Create a new NSM-backed RNG.
    ///
    /// On Linux, this opens an NSM session for random number generation.
    /// On other platforms, this falls back to the thread RNG.
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
        Some(Self {
            inner: rand::thread_rng(),
        })
    }

    /// Create a fallback RNG using the OS random source.
    #[cfg(not(target_os = "linux"))]
    pub fn fallback() -> Self {
        Self {
            inner: rand::thread_rng(),
        }
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
            // Fallback to OS RNG if NSM is not available
            rand::rngs::OsRng.fill_bytes(dest);
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
                    // Fallback to OS RNG on error
                    rand::rngs::OsRng.fill_bytes(&mut dest[filled..]);
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
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.fill_bytes(dest);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.inner.try_fill_bytes(dest)
    }
}

// Mark NsmRng as cryptographically secure
impl CryptoRng for NsmRng {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nsm_rng_fill_bytes() {
        let mut rng = NsmRng::default();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        // On non-Linux or without NSM, should still fill with random bytes
        // We can't test for specific values, but we can test that it doesn't panic
    }

    #[test]
    fn test_nsm_rng_next_u64() {
        let mut rng = NsmRng::default();
        let _ = rng.next_u64();
    }
}
