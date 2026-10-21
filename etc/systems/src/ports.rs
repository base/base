//! Cross-process port allocation for in-process system-test nodes.

use std::{
    fs::{self, File, OpenOptions},
    net::{Ipv4Addr, TcpListener, UdpSocket},
    path::PathBuf,
    sync::Mutex,
};

/// Claims in the current process, held until it exits so the file locks outlive the call.
static CLAIMED: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// A pool of loopback ports that concurrent test processes claim without colliding.
///
/// Picking a port by binding port 0 and releasing it races: another process, or the kernel handing
/// out an ephemeral port, can take it before the node binds it. Nextest runs every test in its own
/// process, so an in-process counter cannot coordinate them either. Instead each port in a fixed
/// range has a lock file, and a port is claimed by locking its file. The lock is held until the
/// process exits, which is as long as any node in a test needs it.
///
/// The range sits below the kernel's default ephemeral range, so ports handed out by the OS for
/// outgoing connections or port-0 binds cannot land in it. Each port is reserved for both TCP and
/// UDP.
#[derive(Debug, Clone, Copy)]
pub struct PortPool;

impl PortPool {
    /// First port in the pool.
    const FIRST: u16 = 20_000;
    /// Number of ports in the pool.
    const LEN: u16 = 10_000;

    /// Claims a port that no other process using the pool holds and that is free to bind now.
    ///
    /// # Panics
    ///
    /// Panics if every port in the pool is taken, or the lock directory cannot be created.
    pub fn claim() -> u16 {
        let dir = Self::lock_dir();
        fs::create_dir_all(&dir).expect("failed to create the system-test port lock directory");

        // Start at a random offset so concurrent processes do not all contend for the same ports.
        let start = rand::random::<u16>() % Self::LEN;
        for step in 0..Self::LEN {
            let port = Self::FIRST + (start + step) % Self::LEN;
            let Ok(lock) = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("{port}.lock")))
            else {
                continue;
            };
            if lock.try_lock().is_err() || !Self::is_free(port) {
                continue;
            }
            CLAIMED.lock().expect("claimed ports lock poisoned").push(lock);
            return port;
        }
        panic!("all {} system-test pool ports are in use", Self::LEN);
    }

    /// Returns whether nothing outside the pool is listening on `port` over TCP or UDP.
    fn is_free(port: u16) -> bool {
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
            && UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
    }

    fn lock_dir() -> PathBuf {
        std::env::temp_dir().join("base-system-tests-ports")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        process::{Command, Stdio},
        thread,
        time::Duration,
    };

    use super::PortPool;

    /// Set in the child processes spawned by `concurrent_processes_claim_disjoint_ports`.
    const CHILD_ENV: &str = "BASE_SYSTEM_TESTS_PORT_POOL_CHILD";
    const PORTS_PER_PROCESS: usize = 40;
    const PROCESSES: usize = 6;

    #[test]
    fn claims_are_unique_and_inside_the_pool() {
        let ports: Vec<u16> = (0..64).map(|_| PortPool::claim()).collect();
        assert_eq!(ports.iter().collect::<HashSet<_>>().len(), ports.len());
        assert!(
            ports
                .iter()
                .all(|port| (PortPool::FIRST..PortPool::FIRST + PortPool::LEN).contains(port))
        );
    }

    #[test]
    fn threads_claim_disjoint_ports() {
        let handles: Vec<_> = (0..8)
            .map(|_| thread::spawn(|| (0..32).map(|_| PortPool::claim()).collect::<Vec<_>>()))
            .collect();
        let ports: Vec<u16> =
            handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect();
        assert_eq!(ports.iter().collect::<HashSet<_>>().len(), ports.len());
    }

    /// Claims ports and keeps them held, so sibling processes overlap in time. Does nothing unless
    /// spawned by `concurrent_processes_claim_disjoint_ports`.
    #[test]
    fn claims_ports_for_the_parent_process() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let ports: Vec<String> =
            (0..PORTS_PER_PROCESS).map(|_| PortPool::claim().to_string()).collect();
        println!("CLAIMED {}", ports.join(","));
        thread::sleep(Duration::from_secs(3));
    }

    #[test]
    fn concurrent_processes_claim_disjoint_ports() {
        if std::env::var_os(CHILD_ENV).is_some() {
            return;
        }
        let children: Vec<_> = (0..PROCESSES)
            .map(|_| {
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "ports::tests::claims_ports_for_the_parent_process"])
                    .args(["--nocapture", "--test-threads=1"])
                    .env(CHILD_ENV, "1")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();

        let mut ports = Vec::new();
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "child process failed");
            let stdout = String::from_utf8(output.stdout).unwrap();
            // libtest prints the test name on the same line, so search for the marker.
            let claimed = stdout.split_once("CLAIMED ").unwrap().1.lines().next().unwrap();
            ports.extend(claimed.split(',').map(|port| port.parse::<u16>().unwrap()));
        }
        assert_eq!(ports.len(), PROCESSES * PORTS_PER_PROCESS);
        assert_eq!(ports.iter().collect::<HashSet<_>>().len(), ports.len());
    }
}
