//! Shared RPC context for typed workloads against an owned Docker devnet.

use std::{path::Path, time::Duration};

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionReceipt;
use eyre::{Result, WrapErr};
use serde_json::json;
use tokio::time::{Instant, sleep, timeout_at};

use crate::{AcceptanceCheck, CheckResult, EndpointMap, Provisioner, Rpc, ScenarioConfig, Status};

/// Borrowed managed-devnet configuration and deadline for one typed workload.
#[derive(Debug)]
pub struct WorkloadContext<'a> {
    /// Logical roles resolved by the managed provisioner.
    pub endpoints: &'a EndpointMap,
    /// Validated scenario, including chain identities and fork schedules.
    pub config: &'a ScenarioConfig,
    /// Invocation-owned output and generated chain artifacts.
    pub output: &'a Path,
    /// Deadline shared by all operations in this workload.
    pub deadline: Instant,
    /// Per-request-bounded JSON-RPC client.
    pub rpc: Rpc,
}

impl WorkloadContext<'_> {
    /// Records a workload outcome while bounding every internal operation by one deadline.
    pub async fn run(&self, check: &AcceptanceCheck, provisioner: &Provisioner) -> CheckResult {
        let started = Instant::now();
        let operation = timeout_at(self.deadline, async {
            match check {
                AcceptanceCheck::Contract { case, .. } => case.execute(self).await,
                AcceptanceCheck::Transaction { case, .. } => case.execute(self).await,
                AcceptanceCheck::Runtime { case, .. } => case.execute(self, provisioner).await,
                _ => eyre::bail!("not a typed devnet workload"),
            }
        })
        .await;
        let (status, observed, message) = match operation {
            Ok(Ok(observed)) => (Status::Passed, observed, "workload assertions passed".into()),
            Ok(Err(error)) => {
                let infrastructure = error.downcast_ref::<reqwest::Error>().is_some()
                    || error.downcast_ref::<std::io::Error>().is_some()
                    || error.downcast_ref::<serde_json::Error>().is_some()
                    || error.downcast_ref::<alloy_transport::TransportError>().is_some()
                    || error.downcast_ref::<tokio::time::error::Elapsed>().is_some();
                (
                    if infrastructure { Status::Error } else { Status::Failed },
                    json!({}),
                    provisioner.sanitize(&format!("{error:#}")),
                )
            }
            Err(_) => (
                Status::Error,
                json!({}),
                "workload deadline elapsed; completion was not observed".into(),
            ),
        };
        CheckResult {
            id: check.id().into(),
            kind: check.kind().into(),
            status,
            duration_ms: started.elapsed().as_millis() as u64,
            expected: json!({"configuration": check}),
            observed,
            message,
            next_step: if status == Status::Passed {
                "no action required"
            } else {
                "inspect workload evidence and owned Compose logs; rerun against fresh state"
            }
            .into(),
            samples: 0,
            rpc_errors: u64::from(status == Status::Error),
            evidence: Vec::new(),
        }
    }

    /// Resolves a required endpoint without exposing arbitrary caller-supplied destinations.
    pub fn endpoint(&self, role: &str) -> Result<&str> {
        self.endpoints
            .get(role)
            .map(String::as_str)
            .ok_or_else(|| eyre::eyre!("missing workload endpoint {role}"))
    }

    /// Creates a typed provider for an execution role in this devnet.
    pub fn provider(&self, role: &str) -> Result<RootProvider<Base>> {
        Ok(RootProvider::new_http(self.endpoint(role)?.parse()?))
    }

    /// Derives a public development-only account funded by the canonical genesis.
    pub fn signer(index: u32) -> Result<PrivateKeySigner> {
        if index > 9 {
            eyre::bail!("only the ten canonical development accounts are supported");
        }
        Ok(MnemonicBuilder::<English>::default()
            .phrase("test test test test test test test test test test test junk")
            .index(index)?
            .build()?)
    }

    /// Observes a receipt without resubmitting a potentially accepted transaction.
    pub async fn receipt(&self, role: &str, hash: B256) -> Result<BaseTransactionReceipt> {
        let provider = self.provider(role)?;
        timeout_at(self.deadline, async {
            loop {
                if let Some(receipt) = provider.get_transaction_receipt(hash).await? {
                    return Ok(receipt);
                }
                sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .wrap_err_with(|| format!("receipt deadline elapsed on {role} for {hash}"))?
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use tokio::net::TcpListener;

    use super::*;

    fn fixture() -> (ScenarioConfig, tempfile::TempDir) {
        let config = ScenarioConfig::load(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/system/runtime/sync.toml"),
        )
        .unwrap();
        (config, tempfile::tempdir().unwrap())
    }

    fn provisioner(output: &Path) -> Provisioner {
        Provisioner::new(Path::new(env!("CARGO_MANIFEST_DIR")).into(), output.into(), "test")
    }

    #[tokio::test]
    async fn dispatch_reports_assertion_failure() {
        let (mut config, output) = fixture();
        let check = config.checks[1].clone();
        config
            .devnet
            .l2
            .forks
            .insert("denim".into(), crate::ForkActivation::AtBlock { at_block: 0 });
        let endpoints = BTreeMap::from([
            ("builder".into(), "http://127.0.0.1:1".into()),
            ("validator".into(), "http://127.0.0.1:1".into()),
        ]);
        let context = WorkloadContext {
            endpoints: &endpoints,
            config: &config,
            output: output.path(),
            deadline: Instant::now() + Duration::from_secs(1),
            rpc: Rpc::new().unwrap(),
        };

        let result = context.run(&check, &provisioner(output.path())).await;

        assert_eq!(result.status, Status::Failed);
        assert!(result.message.contains("pending_flashblocks requires Denim to be disabled"));
        assert_eq!(result.rpc_errors, 0);
    }

    #[tokio::test]
    async fn dispatch_reports_rpc_infrastructure_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });
        let (config, output) = fixture();
        let check = config.checks[0].clone();
        let endpoint = format!("http://{address}");
        let endpoints =
            BTreeMap::from([("builder".into(), endpoint.clone()), ("validator".into(), endpoint)]);
        let context = WorkloadContext {
            endpoints: &endpoints,
            config: &config,
            output: output.path(),
            deadline: Instant::now() + Duration::from_secs(2),
            rpc: Rpc::new().unwrap(),
        };

        let result = context.run(&check, &provisioner(output.path())).await;
        server.await.unwrap();

        assert_eq!(result.status, Status::Error);
        assert_eq!(result.rpc_errors, 1);
    }

    #[tokio::test]
    async fn dispatch_reports_workload_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let (config, output) = fixture();
        let check = config.checks[0].clone();
        let endpoint = format!("http://{address}");
        let endpoints =
            BTreeMap::from([("builder".into(), endpoint.clone()), ("validator".into(), endpoint)]);
        let context = WorkloadContext {
            endpoints: &endpoints,
            config: &config,
            output: output.path(),
            deadline: Instant::now() + Duration::from_millis(20),
            rpc: Rpc::new().unwrap(),
        };

        let result = context.run(&check, &provisioner(output.path())).await;
        server.abort();

        assert_eq!(result.status, Status::Error);
        // Inner and outer timers share a deadline; either may observe it first.
        assert!(result.message.contains("deadline"));
        assert!(result.duration_ms < 1000, "the hung RPC outlived its workload deadline");
        assert_eq!(result.rpc_errors, 1);
    }
}
