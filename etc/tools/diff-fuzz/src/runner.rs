//! Long-running adapter processes that answer protocol requests over stdin/stdout.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use eyre::{WrapErr, eyre};
use serde::de::DeserializeOwned;

use crate::{Request, Response, TrieResponse};

/// A protocol response, which echoes its request's id.
pub trait HasId {
    /// The echoed request id.
    fn id(&self) -> &str;
}

impl HasId for Response {
    fn id(&self) -> &str {
        &self.id
    }
}

impl HasId for TrieResponse {
    fn id(&self) -> &str {
        &self.id
    }
}

/// How to start one adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSpec {
    /// Short client name used in reports, such as `base` or `nethermind`.
    pub name: String,
    /// Program and arguments.
    pub command: Vec<String>,
}

impl AdapterSpec {
    /// Parses `name=command args...`; the command is split on whitespace.
    pub fn parse(spec: &str) -> eyre::Result<Self> {
        let (name, command) = spec
            .split_once('=')
            .ok_or_else(|| eyre!("adapter spec must be name=command: {spec}"))?;
        let command: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if name.is_empty() || command.is_empty() {
            return Err(eyre!("adapter spec must be name=command: {spec}"));
        }
        Ok(Self { name: name.to_owned(), command })
    }
}

/// Why an adapter produced no response for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFailure {
    /// The process exited or closed stdout.
    Crashed(String),
    /// No response within the timeout; the process was killed.
    TimedOut,
    /// The response line was not valid protocol JSON or had the wrong id.
    BadResponse(String),
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crashed(why) => write!(f, "adapter crashed: {why}"),
            Self::TimedOut => write!(f, "adapter timed out"),
            Self::BadResponse(why) => write!(f, "bad adapter response: {why}"),
        }
    }
}

/// One running adapter process. It is restarted after a crash, timeout, or bad response.
#[derive(Debug)]
pub struct Adapter {
    /// How the process is started.
    pub spec: AdapterSpec,
    /// Per-request timeout.
    pub timeout: Duration,
    process: Option<Running>,
}

#[derive(Debug)]
struct Running {
    child: Child,
    /// `None` while a writer thread holds it.
    stdin: Option<ChildStdin>,
    lines: Receiver<std::io::Result<String>>,
}

impl Adapter {
    /// Creates an adapter handle; the process starts on the first call.
    pub const fn new(spec: AdapterSpec, timeout: Duration) -> Self {
        Self { spec, timeout, process: None }
    }

    fn start(&self) -> eyre::Result<Running> {
        let mut child = Command::new(&self.spec.command[0])
            .args(&self.spec.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Its own process group, so a restart also ends processes a wrapper forked.
            .process_group(0)
            .spawn()
            .wrap_err_with(|| format!("starting adapter {}", self.spec.name))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::with_capacity(1 << 20, stdout);
            loop {
                let mut line = String::new();
                let result =
                    reader.read_line(&mut line).map(|n| if n == 0 { None } else { Some(line) });
                let stop = !matches!(result, Ok(Some(_)));
                let sent = match result {
                    Ok(Some(line)) => tx.send(Ok(line)),
                    Ok(None) => tx.send(Err(std::io::Error::other("stdout closed"))),
                    Err(err) => tx.send(Err(err)),
                };
                if stop || sent.is_err() {
                    break;
                }
            }
        });
        Ok(Running { child, stdin: Some(stdin), lines })
    }

    /// Ends the process; the next call starts a new one.
    pub fn restart(&mut self) {
        self.kill();
    }

    fn kill(&mut self) {
        if let Some(mut running) = self.process.take() {
            Self::kill_group(&mut running.child);
        }
    }

    fn kill_group(child: &mut Child) {
        // The adapter leads its process group (pgid = pid), so kill(-pid) reaches every process
        // in it.
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: kill has no memory-safety preconditions.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = child.wait();
    }

    /// Sends one serialized request line and waits for its response.
    pub fn call_line(&mut self, id: &str, line: &str) -> Result<Response, CallFailure> {
        self.call_as(id, line)
    }

    /// Sends one serialized request line and waits for a response of type `R` with id `id`.
    pub fn call_as<R: DeserializeOwned + HasId>(
        &mut self,
        id: &str,
        line: &str,
    ) -> Result<R, CallFailure> {
        if self.process.is_none() {
            self.process =
                Some(self.start().map_err(|err| CallFailure::Crashed(format!("{err:#}")))?);
        }
        let running = self.process.as_mut().expect("process started above");
        let deadline = Instant::now() + self.timeout;
        // Requests are far larger than a pipe buffer, so an adapter that stops reading would
        // block the write forever; write from a thread and wait for it under the deadline.
        let mut stdin = running.stdin.take().expect("stdin returns after every write");
        let payload = format!("{line}\n");
        let (done, written) = mpsc::channel();
        thread::spawn(move || {
            let result = stdin.write_all(payload.as_bytes()).and_then(|()| stdin.flush());
            let _ = done.send((stdin, result));
        });
        match written.recv_timeout(self.timeout) {
            Ok((stdin, Ok(()))) => running.stdin = Some(stdin),
            Ok((_, Err(err))) => {
                let status = running.child.try_wait().ok().flatten();
                self.kill();
                return Err(CallFailure::Crashed(format!(
                    "writing request: {err} (exit status {status:?})"
                )));
            }
            Err(_) => {
                // Killing the adapter closes the pipe, which ends the writer thread.
                self.kill();
                return Err(CallFailure::TimedOut);
            }
        }
        let received =
            running.lines.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let line = match received {
            Ok(Ok(line)) => line,
            Ok(Err(err)) => {
                // The adapter may still be alive, for example after a non-UTF-8 line stops the
                // reader, so read the status without blocking and kill it.
                let status = running.child.try_wait().ok().flatten();
                self.kill();
                return Err(CallFailure::Crashed(format!("{err} (exit status {status:?})")));
            }
            Err(RecvTimeoutError::Timeout) => {
                self.kill();
                return Err(CallFailure::TimedOut);
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.kill();
                return Err(CallFailure::Crashed("reader thread stopped".to_owned()));
            }
        };
        match serde_json::from_str::<R>(&line) {
            Ok(response) if response.id() == id => Ok(response),
            Ok(response) => {
                self.kill();
                Err(CallFailure::BadResponse(format!("expected id {id}, got {}", response.id())))
            }
            Err(err) => {
                self.kill();
                let head: String = line.chars().take(300).collect();
                Err(CallFailure::BadResponse(format!("{err}: {head}")))
            }
        }
    }

    /// Sends one request and waits for its response.
    pub fn call(&mut self, request: &Request) -> Result<Response, CallFailure> {
        let line = serde_json::to_string(request).expect("requests serialize");
        self.call_line(&request.id, &line)
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        if let Some(mut running) = self.process.take() {
            // Closing stdin asks the adapter to exit; kill it if it does not.
            drop(running.stdin);
            for _ in 0..20 {
                if matches!(running.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Self::kill_group(&mut running.child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_utf8_response_from_live_adapter_is_a_crash_not_a_hang() {
        let spec = AdapterSpec {
            name: "bad".to_owned(),
            command: ["sh", "-c", "printf '\\377\\n'; exec sleep 60"].map(str::to_owned).to_vec(),
        };
        let mut adapter = Adapter::new(spec, Duration::from_secs(30));
        let started = Instant::now();
        let result = adapter.call_line("x", "{}");
        assert!(matches!(result, Err(CallFailure::Crashed(_))), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn adapter_that_stops_reading_stdin_times_out() {
        // A wrapper that forks rather than execs: the timeout must end its child too.
        let pid_file = std::env::temp_dir().join(format!("diff-fuzz-stuck-{}", std::process::id()));
        let script = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
        let spec = AdapterSpec {
            name: "stuck".to_owned(),
            command: vec!["sh".to_owned(), "-c".to_owned(), script],
        };
        let mut adapter = Adapter::new(spec, Duration::from_secs(1));
        let started = Instant::now();
        let request = "x".repeat(1 << 20);
        assert_eq!(adapter.call_line("x", &request), Err(CallFailure::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let _ = std::fs::remove_file(&pid_file);
        thread::sleep(Duration::from_millis(200));
        let alive = std::path::Path::new(&format!("/proc/{}", pid.trim())).exists();
        assert!(!alive, "the wrapper's child outlived the restart");
    }
}
