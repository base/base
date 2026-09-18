//! Per-case evidence kept before any owned service is shut down.

use std::{fs, path::PathBuf, process::Command};

use base_system_tests::{SetupImage, SystemTestStack, unique_name};
use eyre::{Result, WrapErr, ensure};
use serde_json::{Value, json};

use super::rpc::Rpc;

/// Incremental fail-closed evidence for one execution of an acceptance case.
#[derive(Debug)]
pub struct Evidence {
    /// Fresh case artifact directory; never a shared container or node datadir.
    pub directory: PathBuf,
    /// Evidence document checkpointed after each observable phase.
    pub document: Value,
}

impl Evidence {
    /// Refuses to overwrite results from an earlier run.
    pub fn new(case: &str) -> Result<Self> {
        let directory = std::env::var_os("BASE_GLAMSTERDAM_ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(unique_name("base-glamsterdam")));
        fs::create_dir_all(&directory)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("evidence.json"))
            .wrap_err("acceptance evidence directory already contains a result")?;
        let source = SetupImage::find_repo_root()?;
        let revision =
            Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&source).output()?;
        let dirty =
            Command::new("git").args(["status", "--porcelain=v1"]).current_dir(&source).output()?;
        ensure!(
            revision.status.success() && dirty.status.success(),
            "cannot record source provenance"
        );
        let evidence = Self {
            directory,
            document: json!({
                "schema_version": 1,
                "case": case,
                "status": "failed",
                "phase": "setup",
                "source": {"revision": String::from_utf8(revision.stdout)?.trim(), "dirty_state": String::from_utf8(dirty.stdout)?, "tested_boundary": "container L1 EL/CL; in-process Base EL/CL/batcher, not shipped Base executables"},
                "activation": null,
                "transfers": {"pre": null, "post": null},
                "batches": {"pre": null, "post": null},
                "safe_blocks": {"pre": null, "post": null},
                "l2_rules": null,
                "error": "scenario has not completed"
            }),
        };
        evidence.checkpoint()?;
        eprintln!("Glamsterdam evidence: {}", evidence.directory.display());
        Ok(evidence)
    }

    /// Writes evidence atomically while retaining an earlier valid failure checkpoint on error.
    pub fn checkpoint(&self) -> Result<()> {
        let temporary = self.directory.join("evidence.json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(&self.document)?)?;
        fs::rename(temporary, self.directory.join("evidence.json"))?;
        Ok(())
    }

    /// Copies rendered configurations without recording the fixture's known test private keys.
    pub fn configs(&self, system: &SystemTestStack) -> Result<()> {
        fs::create_dir_all(self.directory.join("configs"))?;
        fs::copy(
            system.l1_genesis().el_genesis_path(),
            self.directory.join("configs/l1-genesis.json"),
        )?;
        fs::copy(system.l1_genesis().cl_config_path(), self.directory.join("configs/beacon.yaml"))?;
        fs::write(
            self.directory.join("configs/l2-genesis.json"),
            system.l2_deployment().read_genesis()?,
        )?;
        fs::write(
            self.directory.join("configs/rollup.json"),
            system.l2_deployment().read_rollup_config()?,
        )?;
        Ok(())
    }

    /// Captures live RPC state, including error responses, before shutdown on success or failure.
    pub async fn rpc_diagnostics(&self, rpc: &Rpc, system: &SystemTestStack) -> Result<()> {
        let mut diagnostics = json!({"batcher_failure": system.l2_stack().batcher().failure()});
        for (name, url) in [
            ("l1", system.l1_rpc_url().await?.to_string()),
            ("sequencer", system.l2_rpc_url()?.to_string()),
            ("verifier", system.l2_client_rpc_url()?.to_string()),
        ] {
            for tag in ["latest", "safe", "finalized"] {
                let response =
                    rpc.response(&url, "eth_getBlockByNumber", json!([tag, false])).await;
                diagnostics[name][tag] = match response {
                    Ok(response) => response,
                    Err(error) => json!({"transport_error": format!("{error:#}")}),
                };
            }
        }
        fs::write(
            self.directory.join("rpc-diagnostics.json"),
            serde_json::to_vec_pretty(&diagnostics)?,
        )?;
        Ok(())
    }
}
