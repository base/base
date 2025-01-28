use std::future::Future;
use std::path::Path;
use std::{
    fs::{File, OpenOptions},
    io,
    io::prelude::*,
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, SystemTime},
};
use time::{format_description, OffsetDateTime};
use tokio::time::sleep;

/// Default JWT token for testing purposes
pub const DEFAULT_JWT_TOKEN: &str =
    "688f5d737bad920bdfb2fc2f488d6b6209eebda1dae949a8de91398d932c517a";

mod integration_test;
pub mod op_rbuilder;

#[derive(Debug)]
pub enum IntegrationError {
    SpawnError,
    BinaryNotFound,
    SetupError,
    LogError,
    ServiceAlreadyRunning,
}

pub struct ServiceInstance {
    process: Option<Child>,
    pub log_path: PathBuf,
}

pub struct IntegrationFramework {
    test_dir: PathBuf,
    services: Vec<ServiceInstance>,
}

pub trait Service {
    /// Configure and return the command to run the service
    fn command(&self) -> Command;

    /// Return a future that resolves when the service is ready
    fn ready(&self, log_path: &Path) -> impl Future<Output = Result<(), IntegrationError>> + Send;
}

/// Helper function to poll logs periodically
pub async fn poll_logs(
    log_path: &Path,
    pattern: &str,
    interval: Duration,
    timeout: Duration,
) -> Result<(), IntegrationError> {
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(IntegrationError::SpawnError);
        }

        let mut file = File::open(log_path).map_err(|_| IntegrationError::LogError)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|_| IntegrationError::LogError)?;

        if contents.contains(pattern) {
            return Ok(());
        }

        sleep(interval).await;
    }
}

impl ServiceInstance {
    pub fn new(name: String, test_dir: PathBuf) -> Self {
        let log_path = test_dir.join(format!("{}.log", name));
        Self {
            process: None,
            log_path,
        }
    }

    pub fn start(&mut self, command: Command) -> Result<(), IntegrationError> {
        if self.process.is_some() {
            return Err(IntegrationError::ServiceAlreadyRunning);
        }

        let log = open_log_file(&self.log_path)?;
        let stdout = log.try_clone().map_err(|_| IntegrationError::LogError)?;
        let stderr = log.try_clone().map_err(|_| IntegrationError::LogError)?;

        let mut cmd = command;
        cmd.stdout(stdout).stderr(stderr);

        let child = match cmd.spawn() {
            Ok(child) => Ok(child),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Err(IntegrationError::BinaryNotFound),
                _ => Err(IntegrationError::SpawnError),
            },
        }?;

        self.process = Some(child);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), IntegrationError> {
        if let Some(mut process) = self.process.take() {
            process.kill().map_err(|_| IntegrationError::SpawnError)?;
        }
        Ok(())
    }

    /// Start a service using its configuration and wait for it to be ready
    pub async fn start_with_config<T: Service>(
        &mut self,
        config: &T,
    ) -> Result<(), IntegrationError> {
        self.start(config.command())?;
        config.ready(&self.log_path).await?;
        Ok(())
    }
}

impl IntegrationFramework {
    pub fn new() -> Result<Self, IntegrationError> {
        let dt: OffsetDateTime = SystemTime::now().into();
        let format = format_description::parse("[year]_[month]_[day]_[hour]_[minute]_[second]")
            .map_err(|_| IntegrationError::SetupError)?;

        let test_name = dt
            .format(&format)
            .map_err(|_| IntegrationError::SetupError)?;

        let mut test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        test_dir.push("../../integration_logs");
        test_dir.push(test_name);

        std::fs::create_dir_all(&test_dir).map_err(|_| IntegrationError::SetupError)?;

        Ok(Self {
            test_dir,
            services: Vec::new(),
        })
    }

    pub async fn start<T: Service>(
        &mut self,
        name: &str,
        config: &T,
    ) -> Result<&mut ServiceInstance, IntegrationError> {
        let service = self.create_service(name)?;
        service.start_with_config(config).await?;
        Ok(service)
    }

    pub fn create_service(&mut self, name: &str) -> Result<&mut ServiceInstance, IntegrationError> {
        let service = ServiceInstance::new(name.to_string(), self.test_dir.clone());
        self.services.push(service);
        Ok(self.services.last_mut().unwrap())
    }
}

fn open_log_file(path: &PathBuf) -> Result<File, IntegrationError> {
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).map_err(|_| IntegrationError::LogError)?;

    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|_| IntegrationError::LogError)
}

impl Drop for IntegrationFramework {
    fn drop(&mut self) {
        for service in &mut self.services {
            let _ = service.stop();
        }
    }
}
