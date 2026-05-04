use thiserror::Error;

/// Errors produced by the benchmark orchestrator.
#[derive(Debug, Error)]
pub enum BenchmarkError {
    /// Configuration or matrix-expansion error.
    #[error("config error: {0}")]
    Config(String),

    /// I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// EL client lifecycle error.
    #[error("client error: {0}")]
    Client(String),

    /// Engine API call failed.
    #[error("engine API error: {0}")]
    EngineApi(String),

    /// Metrics collection or scraping error.
    #[error("metrics error: {0}")]
    Metrics(String),

    /// RPC proxy error.
    #[error("proxy error: {0}")]
    Proxy(String),

    /// Snapshot creation or lookup error.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// Operation timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Child process exited unexpectedly.
    #[error("process {binary} crashed (exit code: {exit_code:?})")]
    ProcessCrash {
        /// Name of the binary that crashed.
        binary: String,
        /// OS exit code if available.
        exit_code: Option<i32>,
    },

    /// JSON serialization/deserialization error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// YAML parse error.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),

    /// HTTP request error.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
