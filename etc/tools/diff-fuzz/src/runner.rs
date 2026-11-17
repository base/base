//! Long-running adapter processes that answer protocol requests over stdin/stdout.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
};

use eyre::{WrapErr, eyre};

use crate::{Request, Response};

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
    stdin: ChildStdin,
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
        Ok(Running { child, stdin, lines })
    }

    fn kill(&mut self) {
        if let Some(mut running) = self.process.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    /// Sends one serialized request line and waits for its response.
    pub fn call_line(&mut self, id: &str, line: &str) -> Result<Response, CallFailure> {
        if self.process.is_none() {
            self.process =
                Some(self.start().map_err(|err| CallFailure::Crashed(format!("{err:#}")))?);
        }
        let running = self.process.as_mut().expect("process started above");
        let written = running
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| running.stdin.write_all(b"\n"))
            .and_then(|()| running.stdin.flush());
        if let Err(err) = written {
            let status = running.child.try_wait().ok().flatten();
            self.kill();
            return Err(CallFailure::Crashed(format!(
                "writing request: {err} (exit status {status:?})"
            )));
        }
        let received = running.lines.recv_timeout(self.timeout);
        let line = match received {
            Ok(Ok(line)) => line,
            Ok(Err(err)) => {
                let status = running.child.wait().ok();
                self.process = None;
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
        match serde_json::from_str::<Response>(&line) {
            Ok(response) if response.id == id => Ok(response),
            Ok(response) => {
                self.kill();
                Err(CallFailure::BadResponse(format!("expected id {id}, got {}", response.id)))
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
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}
