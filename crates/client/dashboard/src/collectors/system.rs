//! System resource statistics collector.

use std::time::Instant;

use sysinfo::{Pid, System};

use crate::types::SystemData;

/// Collector for system resource statistics.
#[derive(Debug)]
pub(crate) struct SystemCollector {
    /// System info handle.
    sys: System,
    /// Process start time.
    start_time: Instant,
    /// Current process ID.
    pid: Pid,
}

impl Default for SystemCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemCollector {
    /// Creates a new system collector.
    pub(crate) fn new() -> Self {
        Self {
            sys: System::new_all(),
            start_time: Instant::now(),
            pid: Pid::from_u32(std::process::id()),
        }
    }

    /// Collects current system statistics.
    pub(crate) fn collect(&mut self) -> SystemData {
        self.sys.refresh_cpu_all();
        self.sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[self.pid]), true);

        // Get process-specific memory usage
        let working_set = self.sys.process(self.pid).map(|p| p.memory()).unwrap_or(0);

        // CPU usage as fraction (0.0-1.0)
        let cpu_usage = self.sys.global_cpu_usage() / 100.0;

        SystemData {
            uptime: self.start_time.elapsed().as_secs(),
            user_percent: cpu_usage as f64,
            privileged_percent: 0.0, // Kernel CPU time not easily available
            working_set,
        }
    }
}
