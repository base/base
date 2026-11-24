//! Node processes the state campaign starts, stops, kills, and wipes itself.

use std::{
    fs,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use eyre::{WrapErr, bail};
use serde_json::json;

use crate::nodes::{NodeClient, Reply};

/// One node process, launched as `<launch> <name> <datadir>` in its own process group. The
/// launch script maps the client name to its command line; the datadir is `<root>/<name>`.
#[derive(Debug)]
pub struct NodeProcess {
    /// Client name, as in [`NodeClient::name`].
    pub name: String,
    /// Launch script.
    pub launch: PathBuf,
    /// Directory holding every node's datadir and log.
    pub root: PathBuf,
    /// Fault injection the next launch arms (see [`Self::FAULT_ENV`]).
    pub fault: Option<String>,
    /// Peers the next launch connects to (see [`Self::PEERS_ENV`]).
    pub peers: Option<String>,
    /// Stateless start the next launch asks for (see [`Self::STATELESS_ENV`]).
    pub stateless: Option<String>,
    child: Option<Child>,
    /// Length of the log when the running process was launched.
    log_start: u64,
}

impl NodeProcess {
    /// How long a node may take to answer JSON-RPC after starting.
    pub const START_TIMEOUT: Duration = Duration::from_secs(240);
    /// How long a clean stop may take before the node is killed.
    pub const STOP_TIMEOUT: Duration = Duration::from_secs(120);

    /// A process handle for node `name`; nothing is started.
    pub const fn new(name: String, launch: PathBuf, root: PathBuf) -> Self {
        Self {
            name,
            launch,
            root,
            fault: None,
            peers: None,
            stateless: None,
            child: None,
            log_start: 0,
        }
    }

    /// Environment variable passed to the launch script: the fault injection to arm for this
    /// run, in tiger's `TIGER_FAULT` syntax (`seed=N,crash_after_writes=K,mode=torn|lose_unsynced`),
    /// or empty for none. Clients without fault injection ignore it.
    pub const FAULT_ENV: &str = "BASE_DIFF_FAULT";
    /// Environment variable passed to the launch script: comma-separated enodes the node
    /// should peer with (the sync check's source), or empty for none.
    pub const PEERS_ENV: &str = "BASE_DIFF_PEERS";
    /// Environment variable passed to the launch script: `<upstream http url> <start block or
    /// tag>` for a node that starts without state from an upstream's execution witnesses
    /// (tiger's `--stateless.upstream` and `--stateless.start`), or empty for a full node.
    pub const STATELESS_ENV: &str = "BASE_DIFF_STATELESS";
    /// Line prefix a node prints when an injected fault crashes it (tiger's `TIGER_FAULT`).
    pub const FAULT_LINE: &str = "tiger-fault: crash at write";

    /// The log path.
    pub fn log_path(&self) -> PathBuf {
        self.root.join(format!("{}.log", self.name))
    }

    /// The fault line the current run printed, if an injected fault crashed it. The line comes
    /// before the abort, so the process may not have exited yet: its requests already fail.
    pub fn injected_crash(&mut self) -> Option<String> {
        self.child.as_ref()?;
        let text = fs::read(self.log_path()).ok()?;
        let since = text.get(self.log_start as usize..)?;
        String::from_utf8_lossy(since)
            .lines()
            .find(|line| line.contains(Self::FAULT_LINE))
            .map(str::to_owned)
    }

    /// The node's datadir.
    pub fn datadir(&self) -> PathBuf {
        self.root.join(&self.name)
    }

    /// Launches the node, first wiping its datadir when `wipe` is set. The node is killed when
    /// the thread that launched it exits, so launch from a thread that outlives the node (the
    /// campaign's main thread).
    pub fn spawn(&mut self, wipe: bool) -> eyre::Result<()> {
        if self.child.is_some() {
            bail!("{} is already running", self.name);
        }
        let dir = self.datadir();
        if wipe && dir.exists() {
            fs::remove_dir_all(&dir).wrap_err_with(|| format!("wiping {}", dir.display()))?;
        }
        fs::create_dir_all(&dir)?;
        let log = fs::OpenOptions::new().create(true).append(true).open(self.log_path())?;
        self.log_start = log.metadata()?.len();
        let mut command = Command::new(&self.launch);
        command
            .arg(&self.name)
            .arg(&dir)
            .env(Self::FAULT_ENV, self.fault.as_deref().unwrap_or_default())
            .env(Self::PEERS_ENV, self.peers.as_deref().unwrap_or_default())
            .env(Self::STATELESS_ENV, self.stateless.as_deref().unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0);
        // SAFETY: prctl is async-signal-safe. The node dies with the launching thread, so a
        // killed campaign leaves no node holding its ports.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            })
        };
        let child = command.spawn().wrap_err_with(|| format!("launching {}", self.name))?;
        self.child = Some(child);
        Ok(())
    }

    /// Waits until `client` answers `eth_blockNumber`, failing when the node exits first or
    /// takes longer than [`Self::START_TIMEOUT`].
    pub fn wait_ready(&mut self, client: &NodeClient) -> eyre::Result<Duration> {
        let started = Instant::now();
        let probe = json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": [] });
        while started.elapsed() < Self::START_TIMEOUT {
            if !self.alive() {
                bail!("{} exited while starting", self.name);
            }
            if let Reply::Body(body) = client.send(&probe.to_string())
                && body.get("result").is_some()
            {
                return Ok(started.elapsed());
            }
            thread::sleep(Duration::from_millis(250));
        }
        bail!("{} did not answer within {:?}", self.name, Self::START_TIMEOUT)
    }

    /// Stops the node: `SIGTERM` to its process group and a wait (killing it after
    /// [`Self::STOP_TIMEOUT`]), or `SIGKILL` at once when `kill` is set. Returns whether a clean
    /// stop finished in time.
    pub fn stop(&mut self, kill: bool) -> bool {
        let Some(mut child) = self.child.take() else {
            return true;
        };
        let group = -(child.id() as i32);
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(group, if kill { libc::SIGKILL } else { libc::SIGTERM }) };
        let started = Instant::now();
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                // Children of the launch script may outlive it; reach them through the group.
                // SAFETY: as above.
                unsafe { libc::kill(group, libc::SIGKILL) };
                return true;
            }
            if started.elapsed() > Self::STOP_TIMEOUT {
                // SAFETY: as above.
                unsafe { libc::kill(group, libc::SIGKILL) };
                let _ = child.wait();
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether the process is still running.
    pub fn alive(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        self.stop(true);
    }
}
