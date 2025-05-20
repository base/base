use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::{Child, Command},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Binary not found")]
    BinaryNotFound,

    #[error("Failed to spawn process")]
    Spawn(ErrorKind),

    #[error("Failed initialize log streams")]
    Logs,

    #[error("Service is already running")]
    ServiceAlreadyRunning,
}

pub struct ServiceInstance {
    process: Option<Child>,
    pub log_path: PathBuf,
}

impl ServiceInstance {
    pub fn new(name: String, test_dir: PathBuf) -> Self {
        let log_path = test_dir.join(format!("{name}.log"));
        Self {
            process: None,
            log_path,
        }
    }

    pub fn start(&mut self, command: Command) -> Result<(), Error> {
        if self.process.is_some() {
            return Err(Error::ServiceAlreadyRunning);
        }

        let log = open_log_file(&self.log_path)?;
        let stdout = log.try_clone().map_err(|_| Error::Logs)?;
        let stderr = log.try_clone().map_err(|_| Error::Logs)?;

        let mut cmd = command;
        cmd.stdout(stdout).stderr(stderr);

        let child = match cmd.spawn() {
            Ok(child) => Ok(child),
            Err(e) => match e.kind() {
                ErrorKind::NotFound => Err(Error::BinaryNotFound),
                e => Err(Error::Spawn(e)),
            },
        }?;

        self.process = Some(child);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        if let Some(mut process) = self.process.take() {
            return process.kill().map_err(|e| Error::Spawn(e.kind()));
        }
        Ok(())
    }

    /// Start a service using its configuration and wait for it to be ready
    pub async fn start_with_config<T: Service>(&mut self, config: &T) -> Result<(), Error> {
        self.start(config.command())?;
        config.ready(&self.log_path).await?;
        Ok(())
    }

    pub async fn find_log_line(&self, pattern: &str) -> eyre::Result<()> {
        let mut file =
            File::open(&self.log_path).map_err(|_| eyre::eyre!("Failed to open log file"))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|_| eyre::eyre!("Failed to read log file"))?;

        if contents.contains(pattern) {
            Ok(())
        } else {
            Err(eyre::eyre!("Pattern not found in log file: {}", pattern))
        }
    }
}

pub struct IntegrationFramework;

pub trait Service {
    /// Configure and return the command to run the service
    fn command(&self) -> Command;

    /// Return a future that resolves when the service is ready
    fn ready(&self, log_path: &Path) -> impl Future<Output = Result<(), Error>> + Send;
}

fn open_log_file(path: &PathBuf) -> Result<File, Error> {
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).map_err(|_| Error::Logs)?;

    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|_| Error::Logs)
}
