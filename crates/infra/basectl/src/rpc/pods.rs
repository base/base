//! Kubernetes pod polling helpers for basectl.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::sync::mpsc;

use crate::config::{PodGroupConfig, PodsConfig};

/// Live status row for a single Kubernetes pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodStatus {
    /// Pod name from `kubectl get pods`.
    pub name: String,
    /// Ready container count, e.g. `2/2`.
    pub ready: String,
    /// Kubernetes pod status.
    pub status: String,
    /// Restart count as rendered by `kubectl`.
    pub restarts: String,
    /// Pod age as rendered by `kubectl`.
    pub age: String,
}

/// Live status snapshot for one configured Kubernetes pod group.
#[derive(Debug, Clone)]
pub struct PodGroupStatus {
    /// Group definition used for this query.
    pub group: PodGroupConfig,
    /// Pods returned by Kubernetes for this group.
    pub pods: Vec<PodStatus>,
    /// Query error, if this group could not be fetched.
    pub error: Option<String>,
}

/// Latest Kubernetes pods snapshot consumed by the pods view.
#[derive(Debug, Clone)]
pub struct PodsSnapshot {
    /// Per-group pod status.
    pub groups: Vec<PodGroupStatus>,
    /// Local time when this snapshot was refreshed.
    pub refreshed_at: chrono::DateTime<chrono::Local>,
}

/// Kubernetes pod polling helpers.
#[derive(Debug)]
pub struct PodsPoller;

impl PodsPoller {
    /// Builds one pods snapshot by querying each configured Kubernetes group.
    pub async fn snapshot(config: &PodsConfig) -> PodsSnapshot {
        let kubectl = config.kubectl_program();
        let statuses = futures::future::join_all(config.groups.clone().into_iter().map(|group| {
            let kubectl = kubectl.clone();
            async move { Self::fetch_group(&kubectl, group).await }
        }))
        .await;

        PodsSnapshot { groups: statuses, refreshed_at: chrono::Local::now() }
    }

    /// Queries Kubernetes for one pod group.
    pub async fn fetch_group(kubectl: &Path, group: PodGroupConfig) -> PodGroupStatus {
        const KUBECTL_TIMEOUT: Duration = Duration::from_secs(15);

        let mut command = tokio::process::Command::new(Self::expand_user_path(kubectl));
        command.args([
            "--context",
            &group.context,
            "--namespace",
            &group.namespace,
            "get",
            "pods",
            "--no-headers",
        ]);
        if let Some(selector) = group.selector.as_ref() {
            command.args(["-l", selector]);
        }

        let output = tokio::time::timeout(KUBECTL_TIMEOUT, command.output()).await;
        match output {
            Ok(Ok(out)) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                PodGroupStatus { group, pods: Self::parse_kubectl_pods(&stdout), error: None }
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let stdout = String::from_utf8_lossy(&out.stdout);
                let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
                PodGroupStatus {
                    group,
                    pods: Vec::new(),
                    error: Some(format!("kubectl get pods failed: {detail}")),
                }
            }
            Ok(Err(error)) => PodGroupStatus {
                group,
                pods: Vec::new(),
                error: Some(format!("kubectl failed to start: {error}")),
            },
            Err(_) => {
                PodGroupStatus { group, pods: Vec::new(), error: Some("kubectl timed out".into()) }
            }
        }
    }

    /// Parses `kubectl get pods --no-headers` output.
    pub fn parse_kubectl_pods(stdout: &str) -> Vec<PodStatus> {
        stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let name = parts.first()?;
                let ready = parts.get(1).copied().unwrap_or("-");
                let status = parts.get(2).copied().unwrap_or("-");
                let restarts = parts.get(3).copied().unwrap_or("-");
                let age = parts.last().copied().unwrap_or("-");
                Some(PodStatus {
                    name: (*name).to_string(),
                    ready: ready.to_string(),
                    status: status.to_string(),
                    restarts: restarts.to_string(),
                    age: age.to_string(),
                })
            })
            .collect()
    }

    /// Expands a leading `~` in a user-configured executable path.
    pub fn expand_user_path(path: &Path) -> PathBuf {
        let Some(raw) = path.to_str() else { return path.to_path_buf() };
        if raw == "~" {
            return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
        }
        let Some(rest) = raw.strip_prefix("~/") else { return path.to_path_buf() };
        dirs::home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(rest))
    }
}

/// Polls Kubernetes pods at the configured interval and forwards snapshots.
pub async fn run_pods_poller(config: PodsConfig, tx: mpsc::Sender<PodsSnapshot>) {
    let mut interval = tokio::time::interval(config.refresh_interval());
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let snapshot = PodsPoller::snapshot(&config).await;
        if tx.send(snapshot).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PodsPoller;

    #[test]
    fn parses_kubectl_pod_rows() {
        let pods = PodsPoller::parse_kubectl_pods(
            "example-a 2/2 Running 0 4d\nexample-b 1/2 CrashLoopBackOff 7 (1m ago) 2h\n",
        );

        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].name, "example-a");
        assert_eq!(pods[0].ready, "2/2");
        assert_eq!(pods[0].status, "Running");
        assert_eq!(pods[0].restarts, "0");
        assert_eq!(pods[0].age, "4d");
        assert_eq!(pods[1].name, "example-b");
        assert_eq!(pods[1].restarts, "7");
        assert_eq!(pods[1].age, "2h");
    }
}
