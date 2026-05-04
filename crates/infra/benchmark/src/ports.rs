//! TCP port acquisition and release for child process allocation.

use std::collections::HashSet;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use crate::error::BenchmarkError;

const PORT_RANGE_START: u16 = 10_000;
const PORT_RANGE_END: u16 = 65_535;

/// Tracks which ports in the 10000–65535 range are currently in use by
/// benchmark child processes. Uses `TcpListener::bind` to verify availability.
/// Child processes require explicit port numbers in their CLI arguments, so
/// ports must be reserved before the process starts.
#[derive(Debug, Clone, Default)]
pub struct PortManager {
    in_use: Arc<Mutex<HashSet<u16>>>,
}

impl PortManager {
    /// Create a new empty [`PortManager`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire a single available port, marking it as in use.
    pub fn acquire(&self) -> Result<u16, BenchmarkError> {
        let mut in_use = self.in_use.lock().unwrap();
        for port in PORT_RANGE_START..=PORT_RANGE_END {
            if in_use.contains(&port) {
                continue;
            }
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                in_use.insert(port);
                return Ok(port);
            }
        }
        Err(BenchmarkError::Client("no available ports in range 10000–65535".into()))
    }

    /// Acquire `n` available ports atomically, marking all as in use.
    /// On failure, no ports are acquired.
    pub fn acquire_n(&self, n: usize) -> Result<Vec<u16>, BenchmarkError> {
        let mut in_use = self.in_use.lock().unwrap();
        let mut acquired = Vec::with_capacity(n);
        for port in PORT_RANGE_START..=PORT_RANGE_END {
            if acquired.len() == n {
                break;
            }
            if in_use.contains(&port) {
                continue;
            }
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                acquired.push(port);
            }
        }
        if acquired.len() < n {
            return Err(BenchmarkError::Client(format!(
                "could not acquire {n} ports, only found {}",
                acquired.len()
            )));
        }
        for &port in &acquired {
            in_use.insert(port);
        }
        Ok(acquired)
    }

    /// Release a port back to the pool.
    pub fn release(&self, port: u16) {
        self.in_use.lock().unwrap().remove(&port);
    }

    /// Release multiple ports back to the pool.
    pub fn release_all(&self, ports: &[u16]) {
        let mut in_use = self.in_use.lock().unwrap();
        for &port in ports {
            in_use.remove(&port);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_returns_unique_ports() {
        let mgr = PortManager::new();
        let a = mgr.acquire().unwrap();
        let b = mgr.acquire().unwrap();
        assert_ne!(a, b);
        assert!(a >= PORT_RANGE_START);
        assert!(b >= PORT_RANGE_START);
    }

    #[test]
    fn release_allows_reacquisition() {
        let mgr = PortManager::new();
        let port = mgr.acquire().unwrap();
        mgr.release(port);
        let port2 = mgr.acquire().unwrap();
        assert!(port2 >= PORT_RANGE_START);
    }

    #[test]
    fn acquire_n_returns_distinct_ports() {
        let mgr = PortManager::new();
        let ports = mgr.acquire_n(4).unwrap();
        assert_eq!(ports.len(), 4);
        let set: HashSet<_> = ports.iter().collect();
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn acquire_n_all_marked_in_use() {
        let mgr = PortManager::new();
        let ports = mgr.acquire_n(2).unwrap();
        let in_use = mgr.in_use.lock().unwrap();
        for p in &ports {
            assert!(in_use.contains(p));
        }
    }

    #[test]
    fn release_all_clears_ports() {
        let mgr = PortManager::new();
        let ports = mgr.acquire_n(3).unwrap();
        mgr.release_all(&ports);
        let in_use = mgr.in_use.lock().unwrap();
        for p in &ports {
            assert!(!in_use.contains(p));
        }
    }
}
