use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand, ValueEnum};
use eyre::{Context, Result, bail};
use serde_json::json;

use crate::{
    AcceptanceOptions, AcceptanceRunner, Aggregate, CheckResult, CiSuite, ExpectedManifest,
    ExpectedScenario, Provisioner, PublishArgs, Report, RunResult, ScenarioConfig, ScenarioResult,
    StageResult, Status,
};

/// Scenario suites accepted by CI discovery.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SelectionSuite {
    /// Pull-request scenarios only.
    Pr,
    /// Extended scenarios only.
    Extended,
    /// Every scenario.
    All,
}

/// One trusted matrix entry emitted by discovery.
#[derive(Debug, serde::Serialize)]
pub struct MatrixScenario {
    /// Validated scenario identifier.
    pub id: String,
    /// Direct repository-relative TOML path.
    pub path: String,
}

/// `GitHub` matrix document emitted by discovery.
#[derive(Debug, serde::Serialize)]
pub struct ScenarioMatrix {
    /// Selected matrix entries in deterministic path order.
    pub include: Vec<MatrixScenario>,
}

/// Process exit classification used by CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// All checks passed.
    Passed = 0,
    /// One or more assertions failed.
    Assertion = 1,
    /// Invocation, schema, or configuration was invalid.
    Config = 2,
    /// Infrastructure prevented evaluation.
    Infrastructure = 3,
}

/// Acceptance command-line parser and executor.
#[derive(Debug, Parser)]
#[command(name = "base-acceptance")]
pub struct AcceptanceCli {
    /// Operation to perform.
    #[command(subcommand)]
    pub command: AcceptanceCommand,
}

/// Supported acceptance operations.
#[derive(Debug, Subcommand)]
pub enum AcceptanceCommand {
    /// Parse and validate scenario files.
    Validate {
        /// Scenario paths.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Emit a resolved execution plan as JSON.
    Plan {
        /// Scenario paths.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Build an expected-shards manifest.
    Manifest {
        /// Scenario paths.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Run identity.
        #[arg(long)]
        run_id: String,
        /// Tested revision.
        #[arg(long)]
        tested_sha: String,
        /// Destination JSON file.
        #[arg(long)]
        output: PathBuf,
    },
    /// Discover, validate, and select checked-in scenarios for CI.
    Select {
        /// Direct scenario directory.
        #[arg(long, default_value = "acceptance/scenarios")]
        scenarios: PathBuf,
        /// Suite to select.
        #[arg(long, value_enum)]
        suite: SelectionSuite,
        /// Run identity.
        #[arg(long)]
        run_id: String,
        /// Tested revision.
        #[arg(long)]
        tested_sha: String,
        /// Destination expected-manifest JSON file.
        #[arg(long)]
        expected: PathBuf,
        /// Destination matrix JSON file.
        #[arg(long)]
        matrix: PathBuf,
        /// Optional `GitHub` output file receiving the compact matrix.
        #[arg(long)]
        github_output: Option<PathBuf>,
    },
    /// Provision and execute a scenario.
    Run {
        /// Scenario path.
        path: PathBuf,
        /// Output root.
        #[arg(long)]
        output: PathBuf,
        /// Reuse built images without claiming log reuse.
        #[arg(long)]
        no_build: bool,
        /// Run identity.
        #[arg(long)]
        run_id: Option<String>,
        /// Tested revision.
        #[arg(long)]
        tested_sha: Option<String>,
        /// Repository root.
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    /// Attach to endpoints and execute checks without mutation.
    Check {
        /// Scenario path.
        path: PathBuf,
        /// Logical endpoint JSON map.
        #[arg(long)]
        endpoints: PathBuf,
        /// Verified rollup JSON.
        #[arg(long)]
        rollup: Option<PathBuf>,
        /// Output root.
        #[arg(long)]
        output: PathBuf,
        /// Run identity.
        #[arg(long)]
        run_id: Option<String>,
        /// Tested revision.
        #[arg(long)]
        tested_sha: Option<String>,
    },
    /// Render an existing portable result.
    Report {
        /// Input result.json.
        result: PathBuf,
        /// Output report directory.
        #[arg(long)]
        output: PathBuf,
    },
    /// Strictly aggregate sharded results.
    Aggregate {
        /// Expected manifest JSON.
        #[arg(long)]
        expected: PathBuf,
        /// Directory containing result bundles.
        #[arg(long)]
        results: PathBuf,
        /// Output root.
        #[arg(long)]
        output: PathBuf,
    },
    /// Remove resources recorded by an owned acceptance manifest.
    Cleanup {
        /// Ownership manifest produced by a run.
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Publish a trusted, marker-owned `GitHub` PR summary from result artifacts.
    Publish(PublishArgs),
}

/// Inputs for one run or read-only check execution.
#[derive(Debug)]
pub struct CliRun {
    /// Scenario configuration path.
    pub path: PathBuf,
    /// Fresh output root.
    pub output: PathBuf,
    /// Optional explicit run identity.
    pub run_id: Option<String>,
    /// Optional explicit tested revision.
    pub tested_sha: Option<String>,
    /// Repository root.
    pub repo_root: PathBuf,
    /// Endpoints for read-only attach mode.
    pub endpoints: Option<BTreeMap<String, String>>,
    /// Original endpoint-map path for an accurate read-only reproduction command.
    pub endpoint_file: Option<PathBuf>,
    /// Verified rollup configuration for attach mode.
    pub rollup: Option<PathBuf>,
    /// Whether provisioning may build images.
    pub build: bool,
}

impl AcceptanceCli {
    /// Executes the parsed command and returns the stable process code.
    pub async fn execute(self) -> Result<ExitCode> {
        match self.command {
            AcceptanceCommand::Validate { paths } => {
                CliRun::load_all(&paths)?;
                Ok(ExitCode::Passed)
            }
            AcceptanceCommand::Plan { paths } => {
                println!("{}", serde_json::to_string_pretty(&CliRun::load_all(&paths)?)?);
                Ok(ExitCode::Passed)
            }
            AcceptanceCommand::Manifest { paths, run_id, tested_sha, output } => {
                let scenarios = CliRun::load_all(&paths)?
                    .into_iter()
                    .map(|scenario| ExpectedScenario {
                        id: scenario.id,
                        checks: scenario.checks.iter().map(|check| check.id().into()).collect(),
                    })
                    .collect();
                let manifest =
                    ExpectedManifest { schema_version: 1, run_id, tested_sha, scenarios };
                fs::write(output, format!("{}\n", serde_json::to_string_pretty(&manifest)?))?;
                Ok(ExitCode::Passed)
            }
            AcceptanceCommand::Select {
                scenarios,
                suite,
                run_id,
                tested_sha,
                expected,
                matrix,
                github_output,
            } => {
                let (manifest, selected) = CliRun::select(&scenarios, suite, run_id, tested_sha)?;
                let matrix_json = serde_json::to_string(&selected)?;
                fs::write(expected, format!("{}\n", serde_json::to_string_pretty(&manifest)?))?;
                fs::write(matrix, format!("{}\n", serde_json::to_string_pretty(&selected)?))?;
                if let Some(output) = github_output {
                    writeln!(
                        fs::OpenOptions::new().append(true).open(output)?,
                        "matrix={matrix_json}"
                    )?;
                }
                Ok(ExitCode::Passed)
            }
            AcceptanceCommand::Run { path, output, no_build, run_id, tested_sha, repo_root } => {
                let root = CliRun::resolve_repo(repo_root.as_deref().or_else(|| path.parent()))?;
                CliRun {
                    path,
                    output,
                    run_id,
                    tested_sha,
                    repo_root: root,
                    endpoints: None,
                    endpoint_file: None,
                    rollup: None,
                    build: !no_build,
                }
                .execute()
                .await
            }
            AcceptanceCommand::Check { path, endpoints, rollup, output, run_id, tested_sha } => {
                let endpoint_file = endpoints.clone();
                let bytes = fs::read(&endpoints).wrap_err("read endpoint map")?;
                let endpoints: BTreeMap<String, String> =
                    serde_json::from_slice(&bytes).wrap_err("parse endpoint map")?;
                let root = CliRun::resolve_repo(path.parent())?;
                CliRun {
                    path,
                    output,
                    run_id,
                    tested_sha,
                    repo_root: root,
                    endpoints: Some(endpoints),
                    endpoint_file: Some(endpoint_file),
                    rollup,
                    build: false,
                }
                .execute()
                .await
            }
            AcceptanceCommand::Report { result, output } => {
                if fs::metadata(&result)?.len() > 20 * 1024 * 1024 {
                    bail!("result exceeds 20 MiB");
                }

                let run: RunResult = serde_json::from_slice(&fs::read(&result)?)?;
                Report::validate(&run)?;
                CliRun::copy_report_evidence(&result, &output, &run)?;
                Report::write(&run, &output)?;
                Ok(CliRun::verdict(&run))
            }
            AcceptanceCommand::Aggregate { expected, results, output } => {
                let run = Aggregate::write(&expected, &results, &output)?;
                Ok(CliRun::verdict(&run))
            }
            AcceptanceCommand::Cleanup { manifest } => {
                Provisioner::recover(&manifest).await?;
                Ok(ExitCode::Passed)
            }
            AcceptanceCommand::Publish(args) => {
                args.execute().await?;
                Ok(ExitCode::Passed)
            }
        }
    }
}

impl CliRun {
    /// Discovers direct regular TOML files and derives a manifest and matrix together.
    pub fn select(
        directory: &Path,
        suite: SelectionSuite,
        run_id: String,
        tested_sha: String,
    ) -> Result<(ExpectedManifest, ScenarioMatrix)> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(directory).wrap_err("read scenario directory")? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("scenario must be a direct regular file: {}", path.display())
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| eyre::eyre!("scenario filename is not UTF-8"))?;
            if Path::new(&name).components().count() != 1 || name == ".toml" {
                bail!("unsafe scenario filename")
            }
            paths.push((name, path));
        }
        paths.sort_by(|left, right| left.0.cmp(&right.0));
        let mut ids = BTreeSet::new();
        let mut scenarios = Vec::new();
        let mut include = Vec::new();
        for (name, path) in paths {
            let config = ScenarioConfig::load(&path)?;
            if !ids.insert(config.id.clone()) {
                bail!("duplicate scenario id {}", config.id)
            }
            let selected = match suite {
                SelectionSuite::Pr => config.ci.suite == CiSuite::Pr,
                SelectionSuite::Extended => config.ci.suite == CiSuite::Extended,
                SelectionSuite::All => true,
            };
            if selected {
                scenarios.push(ExpectedScenario {
                    id: config.id.clone(),
                    checks: config.checks.iter().map(|check| check.id().into()).collect(),
                });
                include.push(MatrixScenario {
                    id: config.id,
                    path: directory.join(name).to_string_lossy().into_owned(),
                });
            }
        }
        if scenarios.is_empty() {
            bail!("selected suite contains no scenarios")
        }
        Ok((
            ExpectedManifest { schema_version: 1, run_id, tested_sha, scenarios },
            ScenarioMatrix { include },
        ))
    }

    /// Executes one scenario while preserving its invocation identity and start time.
    pub async fn execute(self) -> Result<ExitCode> {
        if self.output.exists() {
            let entries = fs::read_dir(&self.output)?.collect::<std::io::Result<Vec<_>>>()?;
            let only_empty_report = entries.len() == 1
                && entries[0].file_name() == "report"
                && entries[0].file_type()?.is_dir()
                && fs::read_dir(entries[0].path())?.next().is_none();
            if !entries.is_empty() && !only_empty_report {
                bail!("output already exists and is not empty: {}", self.output.display());
            }
        }

        let started_at = Self::now();
        let config = ScenarioConfig::load(&self.path)?;
        if let Some(endpoints) = &self.endpoints {
            AcceptanceRunner::validate_endpoints(&config, endpoints)?;
            if self.rollup.is_none() && config.checks.iter().any(|check| check.start().is_some()) {
                bail!("fork-window checks in attach mode require --rollup");
            }
        }

        fs::create_dir_all(&self.output)?;
        let identity = self.run_id.unwrap_or_else(|| format!("local-{started_at}"));
        let sha = match self.tested_sha {
            Some(sha) => sha,
            None => Self::git_revision(&self.repo_root)?,
        };
        let reproduction = if let Some(path) = &self.endpoint_file {
            format!(
                "base-acceptance check {} --endpoints {}{} --output \"$(mktemp -d)\"",
                Self::shell_quote(&self.path),
                Self::shell_quote(path),
                self.rollup.as_ref().map_or_else(String::new, |path| format!(
                    " --rollup {}",
                    Self::shell_quote(path)
                ))
            )
        } else {
            format!(
                "base-acceptance run {} --output \"$(mktemp -d)\"{}",
                Self::shell_quote(&self.path),
                if self.build { "" } else { " --no-build" }
            )
        };
        let options = AcceptanceOptions {
            repo_root: self.repo_root,
            output: self.output.clone(),
            endpoints: self.endpoints,
            rollup: self.rollup,
            build: self.build,
        };
        let mut scenario = match AcceptanceRunner::run(config.clone(), options).await {
            Ok(result) => result,
            Err(error) => Self::fatal(&config, error.to_string()),
        };
        scenario.reproduction = reproduction;
        let run = RunResult {
            schema_version: 1,
            run_id: identity,
            tested_sha: sha,
            started_at_unix_ms: started_at,
            scenarios: vec![scenario],
        };
        Report::write(&run, &self.output.join("report"))?;
        Ok(Self::verdict(&run))
    }

    /// Loads every supplied scenario configuration.
    pub fn load_all(paths: &[PathBuf]) -> Result<Vec<ScenarioConfig>> {
        paths.iter().map(ScenarioConfig::load).collect()
    }

    /// Creates a truthful blocked result after fatal setup failure.
    pub fn fatal(config: &ScenarioConfig, message: String) -> ScenarioResult {
        let checks = config
            .checks
            .iter()
            .map(|check| CheckResult {
                id: check.id().into(),
                kind: check.kind().into(),
                status: Status::Blocked,
                duration_ms: 0,
                expected: json!({}),
                observed: json!({}),
                message: "fatal setup error prevented evaluation".into(),
                next_step: "inspect setup diagnostic".into(),
                samples: 0,
                rpc_errors: 0,
                evidence: Vec::new(),
            })
            .collect();
        ScenarioResult {
            id: config.id.clone(),
            status: Status::Error,
            duration_ms: 0,
            config: serde_json::to_value(config).unwrap_or_else(|_| json!({})),
            stages: vec![StageResult {
                id: "setup".into(),
                status: Status::Error,
                duration_ms: 0,
                message,
            }],
            checks,
            samples: Vec::new(),
            forks: Vec::new(),
            diagnostics: Vec::new(),
            reproduction: String::new(),
        }
    }

    /// Classifies infrastructure ahead of assertions when both occur.
    pub fn verdict(run: &RunResult) -> ExitCode {
        let infrastructure = run.scenarios.iter().any(|scenario| {
            matches!(scenario.outcome(), Status::Error | Status::Blocked | Status::Cancelled)
        });
        if infrastructure {
            ExitCode::Infrastructure
        } else if run
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.checks)
            .any(|check| check.status == Status::Failed)
        {
            ExitCode::Assertion
        } else if run.passed() {
            ExitCode::Passed
        } else {
            ExitCode::Infrastructure
        }
    }

    /// Finds the containing repository root.
    pub fn resolve_repo(start: Option<&Path>) -> Result<PathBuf> {
        let start = start
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .wrap_err("resolve repository search path")?;
        for ancestor in start.ancestors() {
            if ancestor.join("Cargo.toml").is_file()
                && (ancestor.join("docker-compose.yml").is_file() || ancestor.join(".git").exists())
            {
                return Ok(ancestor.into());
            }
        }
        bail!("could not locate repository root")
    }

    /// Reads the current Git revision and dirty state.
    pub fn git_revision(root: &Path) -> Result<String> {
        let output = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(root).output()?;
        if !output.status.success() {
            bail!("git rev-parse failed");
        }

        let mut sha = String::from_utf8(output.stdout)?.trim().to_owned();
        let dirty =
            Command::new("git").args(["status", "--porcelain"]).current_dir(root).output()?;
        if !dirty.stdout.is_empty() {
            sha.push_str("-dirty");
        }
        Ok(sha)
    }

    /// Quotes one path for inert reproduction text.
    pub fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    /// Returns Unix time in milliseconds.
    pub fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
    }

    /// Copies referenced evidence when rendering an existing portable result.
    pub fn copy_report_evidence(result: &Path, output: &Path, run: &RunResult) -> Result<()> {
        let source = result.parent().unwrap_or_else(|| Path::new("."));
        for evidence in run
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.checks)
            .flat_map(|check| &check.evidence)
        {
            Report::validate_evidence_path(evidence)?;
            let from = source.join(evidence);
            let destination = output.join(evidence);
            let canonical_source = source.canonicalize()?;
            let canonical_file = from
                .canonicalize()
                .wrap_err_with(|| format!("referenced evidence unavailable: {evidence}"))?;
            if !canonical_file.starts_with(canonical_source)
                || fs::symlink_metadata(&from)?.file_type().is_symlink()
            {
                bail!("unsafe evidence source: {evidence}");
            }
            if destination.exists() && destination.canonicalize()? == canonical_file {
                continue;
            }
            fs::create_dir_all(destination.parent().expect("evidence has parent"))?;
            fs::copy(from, destination)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::*;

    fn copy(directory: &Path, source: &str, name: &str) {
        fs::copy(Path::new("scenarios").join(source), directory.join(name)).unwrap();
    }

    #[test]
    fn discovery_is_deterministic_and_manifest_matches_matrix() {
        let temp = tempfile::tempdir().unwrap();
        copy(temp.path(), "smoke.toml", "z.toml");
        copy(temp.path(), "derivation.toml", "a.toml");
        copy(temp.path(), "denim-transition.toml", "m.toml");
        let (manifest, matrix) =
            CliRun::select(temp.path(), SelectionSuite::All, "run".into(), "sha".into()).unwrap();
        assert_eq!(
            manifest.scenarios.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            ["derivation", "denim-transition", "smoke"]
        );
        assert!(
            manifest
                .scenarios
                .iter()
                .map(|item| &item.id)
                .eq(matrix.include.iter().map(|item| &item.id))
        );
        let (_, pr) =
            CliRun::select(temp.path(), SelectionSuite::Pr, "run".into(), "sha".into()).unwrap();
        assert_eq!(pr.include.len(), 1);
        assert_eq!(pr.include[0].id, "smoke");
        let (_, extended) =
            CliRun::select(temp.path(), SelectionSuite::Extended, "run".into(), "sha".into())
                .unwrap();
        assert_eq!(extended.include.len(), 2);
    }

    #[test]
    fn discovery_finds_new_toml_and_rejects_invalid_duplicate_and_empty_selection() {
        let temp = tempfile::tempdir().unwrap();
        copy(temp.path(), "derivation.toml", "new.toml");
        let (_, matrix) =
            CliRun::select(temp.path(), SelectionSuite::Extended, "run".into(), "sha".into())
                .unwrap();
        assert_eq!(matrix.include[0].id, "derivation");
        assert!(
            CliRun::select(temp.path(), SelectionSuite::Pr, "run".into(), "sha".into()).is_err()
        );
        copy(temp.path(), "derivation.toml", "duplicate.toml");
        assert!(
            CliRun::select(temp.path(), SelectionSuite::All, "run".into(), "sha".into()).is_err()
        );
        fs::remove_file(temp.path().join("duplicate.toml")).unwrap();
        fs::write(temp.path().join("invalid.toml"), "not toml").unwrap();
        assert!(
            CliRun::select(temp.path(), SelectionSuite::All, "run".into(), "sha".into()).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_toml_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        symlink(Path::new("../outside.toml"), temp.path().join("linked.toml")).unwrap();
        assert!(
            CliRun::select(temp.path(), SelectionSuite::All, "run".into(), "sha".into()).is_err()
        );
    }
}
