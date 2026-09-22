use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{CheckResult, Report, RunResult, ScenarioResult, StageResult, Status};

const MAX_BYTES: u64 = 20 * 1024 * 1024;
const MAX_RESULTS: usize = 200;
const MAX_DEPTH: usize = 5;

/// Strict description of scenarios expected from a sharded run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Shared run identity, including the workflow attempt.
    pub run_id: String,
    /// Exact tested revision.
    pub tested_sha: String,
    /// Expected scenarios and checks.
    pub scenarios: Vec<ExpectedScenario>,
}

/// One expected scenario and its complete check set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedScenario {
    /// Scenario identifier.
    pub id: String,
    /// Exact expected check identifiers.
    pub checks: Vec<String>,
}

/// Strict result aggregation and bounded evidence relocation.
#[derive(Debug)]
pub struct Aggregate;

impl Aggregate {
    /// Reads one file only after enforcing the per-file 20 `MiB` limit.
    pub fn bounded_read(path: &Path) -> Result<Vec<u8>> {
        let metadata =
            fs::symlink_metadata(path).wrap_err_with(|| format!("inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("input is not a regular file: {}", path.display());
        }
        if metadata.len() > MAX_BYTES {
            bail!("input exceeds 20 MiB: {}", path.display());
        }
        fs::read(path).wrap_err_with(|| format!("read {}", path.display()))
    }

    /// Loads and strictly validates an expected-run manifest.
    pub fn load_expected(path: &Path) -> Result<ExpectedManifest> {
        let expected: ExpectedManifest = serde_json::from_slice(&Self::bounded_read(path)?)
            .wrap_err("parse expected manifest")?;
        if expected.schema_version != 1
            || expected.run_id.is_empty()
            || expected.tested_sha.is_empty()
        {
            bail!("invalid expected manifest identity or schema");
        }
        if expected.scenarios.is_empty() || expected.scenarios.len() > 100 {
            bail!("expected manifest must contain 1..=100 scenarios");
        }
        let mut scenario_ids = BTreeSet::new();
        for scenario in &expected.scenarios {
            Self::validate_id("scenario", &scenario.id)?;
            if !scenario_ids.insert(&scenario.id) {
                bail!("duplicate expected scenario id {}", scenario.id);
            }
            if scenario.checks.is_empty() || scenario.checks.len() > 1_000 {
                bail!("scenario {} has an invalid check count", scenario.id);
            }
            let mut checks = BTreeSet::new();
            for check in &scenario.checks {
                Self::validate_id("check", check)?;
                if !checks.insert(check) {
                    bail!("scenario {} has duplicate check {check}", scenario.id);
                }
            }
        }
        Ok(expected)
    }

    /// Aggregates result bundles and writes result.json, report.html, summary.md and evidence directly.
    /// Invalid or missing manifests still produce an aggregate-error report.
    pub fn write(expected_path: &Path, results: &Path, output: &Path) -> Result<RunResult> {
        fs::create_dir_all(output).wrap_err("create aggregate output")?;
        let expected = match Self::load_expected(expected_path) {
            Ok(expected) => expected,
            Err(error) => {
                let run = Self::fallback(format!("invalid or missing expected manifest: {error}"));
                Report::write(&run, output)?;
                return Ok(run);
            }
        };

        let results_root = match results.canonicalize().wrap_err("resolve results directory") {
            Ok(path) => path,
            Err(error) => {
                let run = Self::run_error(&expected, error.to_string());
                Report::write(&run, output)?;
                return Ok(run);
            }
        };
        let output_absolute = if output.exists() {
            output.canonicalize()?
        } else {
            std::env::current_dir()?.join(output)
        };
        if output_absolute.starts_with(&results_root) {
            let run = Self::run_error(&expected, "aggregate output is inside results input".into());
            Report::write(&run, output)?;
            return Ok(run);
        }

        let mut paths = Vec::new();
        let mut discovery_errors = Vec::new();
        if let Err(error) = Self::discover(&results_root, 0, &mut paths, &mut discovery_errors) {
            let run = Self::run_error(&expected, format!("discover result artifacts: {error}"));
            Report::write(&run, output)?;
            return Ok(run);
        }
        let mut found: BTreeMap<String, Vec<(PathBuf, ScenarioResult)>> = BTreeMap::new();
        let mut errors = discovery_errors;
        for path in paths {
            match Self::bounded_read(&path).and_then(|bytes| {
                let run: RunResult = serde_json::from_slice(&bytes)?;
                Report::validate(&run)?;
                Ok(run)
            }) {
                Ok(run)
                    if run.run_id == expected.run_id && run.tested_sha == expected.tested_sha =>
                {
                    for scenario in run.scenarios {
                        found
                            .entry(scenario.id.clone())
                            .or_default()
                            .push((path.clone(), scenario));
                    }
                }
                Ok(_) => errors.push(format!("stale result identity: {}", path.display())),
                Err(error) => errors.push(format!("invalid result {}: {error}", path.display())),
            }
        }

        let mut scenarios = Vec::new();
        let mut evidence_bytes = 0;
        for wanted in &expected.scenarios {
            let entries = found.remove(&wanted.id).unwrap_or_default();
            if entries.len() != 1 {
                scenarios.push(Self::synthetic(
                    wanted,
                    format!("expected exactly one result, found {}", entries.len()),
                ));
                continue;
            }
            let (source, mut scenario) = entries.into_iter().next().expect("checked one entry");
            let actual: BTreeSet<_> =
                scenario.checks.iter().map(|check| check.id.as_str()).collect();
            let required: BTreeSet<_> = wanted.checks.iter().map(String::as_str).collect();
            if actual != required || scenario.checks.len() != wanted.checks.len() {
                scenarios.push(Self::synthetic(
                    wanted,
                    "result check set does not exactly match manifest".into(),
                ));
                continue;
            }
            scenario.checks.sort_by_key(|check| {
                wanted.checks.iter().position(|id| id == &check.id).expect("validated check set")
            });
            Self::copy_evidence(&source, output, &mut scenario, &mut evidence_bytes);
            scenario.status = scenario.outcome();
            scenarios.push(scenario);
        }
        for id in found.keys() {
            errors.push(format!("unexpected scenario artifact: {id}"));
        }
        if !errors.is_empty() {
            let scenario = scenarios.first_mut().expect("validated manifest is nonempty");
            scenario.diagnostics.push(errors.join("; "));
            scenario.stages.retain(|stage| stage.id != "aggregate-integrity");
            scenario.stages.push(StageResult {
                id: "aggregate-integrity".into(),
                status: Status::Error,
                duration_ms: 0,
                message: "aggregate artifacts were invalid or unexpected; inspect diagnostics"
                    .into(),
            });
            scenario.status = scenario.outcome();
        }
        let run = RunResult {
            schema_version: 1,
            run_id: expected.run_id,
            tested_sha: expected.tested_sha,
            started_at_unix_ms: 0,
            scenarios,
        };
        Report::write(&run, output)?;
        Ok(run)
    }

    /// Discovers result files with hard depth and path-count limits, rejecting symlink artifacts.
    pub fn discover(
        directory: &Path,
        depth: usize,
        paths: &mut Vec<PathBuf>,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            errors.push(format!("result traversal exceeded depth at {}", directory.display()));
            return Ok(());
        }
        for entry in fs::read_dir(directory).wrap_err("read results directory")? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                errors.push(format!("symlink artifact rejected: {}", entry.path().display()));
            } else if ty.is_dir() {
                Self::discover(&entry.path(), depth + 1, paths, errors)?;
            } else if entry.file_name() == "result.json" {
                if paths.len() == MAX_RESULTS {
                    bail!("too many result artifacts");
                }
                paths.push(entry.path());
            }
        }
        Ok(())
    }

    /// Copies evidence only when its canonical source remains in the source bundle and total bytes are bounded.
    /// Source and destination bundles must not be mutated concurrently; path checks do not isolate
    /// aggregation from another process changing the filesystem during a copy.
    pub fn copy_evidence(
        source_result: &Path,
        output: &Path,
        scenario: &mut ScenarioResult,
        copied_bytes: &mut u64,
    ) {
        let Some(bundle) = source_result.parent() else { return };
        let canonical_bundle = match bundle.canonicalize() {
            Ok(path) => path,
            Err(_) => return,
        };
        let mut copied_paths = BTreeSet::new();
        for check in &mut scenario.checks {
            for evidence in &mut check.evidence {
                let original = evidence.clone();
                let relative = PathBuf::from("evidence").join(&scenario.id).join(&original);
                if copied_paths.contains(&original) {
                    *evidence = relative.to_string_lossy().into_owned();
                    continue;
                }
                let source = bundle.join(&original);
                let safe = Report::validate_evidence_path(&original)
                    .and_then(|_| source.canonicalize().map_err(Into::into))
                    .and_then(|canonical| {
                        if canonical.starts_with(&canonical_bundle) {
                            Ok(canonical)
                        } else {
                            bail!("evidence escapes bundle")
                        }
                    })
                    .and_then(|canonical| {
                        let metadata = fs::symlink_metadata(&source)?;
                        if metadata.file_type().is_symlink() || !metadata.is_file() {
                            bail!("evidence is not a regular file")
                        }
                        if *copied_bytes + metadata.len() > MAX_BYTES {
                            bail!("aggregate evidence exceeds 20 MiB")
                        }
                        Ok((canonical, metadata.len()))
                    });
                let copied = safe.and_then(|(canonical, size)| {
                    let destination = output.join(&relative);
                    fs::create_dir_all(destination.parent().expect("evidence has parent"))?;
                    fs::copy(canonical, destination)?;
                    *copied_bytes += size;
                    Ok(())
                });
                match copied {
                    Ok(()) => {
                        copied_paths.insert(original);
                        *evidence = relative.to_string_lossy().into_owned();
                    }
                    Err(error) => {
                        let message = format!("evidence unavailable: {original}: {error}");
                        scenario.diagnostics.push(message.clone());
                        check.status = Status::Error;
                        check.message = message;
                        check.next_step =
                            "inspect scenario artifacts and aggregate diagnostics".into();
                        evidence.clear();
                    }
                }
            }
            check.evidence.retain(|path| !path.is_empty());
        }
    }

    /// Creates a complete nonpassing result when the manifest cannot be trusted.
    pub fn fallback(message: String) -> RunResult {
        RunResult {
            schema_version: 1,
            run_id: "aggregate-error".into(),
            tested_sha: "unknown".into(),
            started_at_unix_ms: 0,
            scenarios: vec![Self::error_scenario("aggregate-error", message)],
        }
    }

    /// Creates a manifest-bound aggregate error result.
    pub fn run_error(expected: &ExpectedManifest, message: String) -> RunResult {
        RunResult {
            schema_version: 1,
            run_id: expected.run_id.clone(),
            tested_sha: expected.tested_sha.clone(),
            started_at_unix_ms: 0,
            scenarios: expected
                .scenarios
                .iter()
                .map(|scenario| Self::synthetic(scenario, message.clone()))
                .collect(),
        }
    }

    /// Creates a blocked representation of one missing or invalid expected scenario.
    pub fn synthetic(expected: &ExpectedScenario, message: String) -> ScenarioResult {
        let checks = expected
            .checks
            .iter()
            .map(|id| CheckResult {
                id: id.clone(),
                kind: "expected".into(),
                status: Status::Blocked,
                duration_ms: 0,
                expected: json!({}),
                observed: json!({}),
                message: message.clone(),
                next_step: "inspect shard result and rerun".into(),
                samples: 0,
                rpc_errors: 0,
                evidence: Vec::new(),
            })
            .collect();
        ScenarioResult {
            id: expected.id.clone(),
            status: Status::Error,
            duration_ms: 0,
            config: json!({}),
            stages: vec![StageResult {
                id: "aggregate".into(),
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

    /// Creates an infrastructure-error scenario describing aggregate integrity failures.
    pub fn error_scenario(id: &str, message: String) -> ScenarioResult {
        ScenarioResult {
            id: id.into(),
            status: Status::Error,
            duration_ms: 0,
            config: json!({}),
            stages: vec![StageResult {
                id: "aggregate".into(),
                status: Status::Error,
                duration_ms: 0,
                message,
            }],
            checks: vec![CheckResult {
                id: "artifact-integrity".into(),
                kind: "aggregate".into(),
                status: Status::Error,
                duration_ms: 0,
                expected: json!({}),
                observed: json!({}),
                message: "aggregate artifacts were invalid or unexpected".into(),
                next_step: "inspect aggregate diagnostics".into(),
                samples: 0,
                rpc_errors: 0,
                evidence: Vec::new(),
            }],
            samples: Vec::new(),
            forks: Vec::new(),
            diagnostics: Vec::new(),
            reproduction: String::new(),
        }
    }

    /// Validates identifiers before they can become report paths or anchors.
    pub fn validate_id(kind: &str, value: &str) -> Result<()> {
        if value.is_empty()
            || value.len() > 100
            || value == "."
            || value == ".."
            || value.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            || Path::new(value).components().any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("unsafe {kind} id {value:?}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn expected() -> ExpectedManifest {
        ExpectedManifest {
            schema_version: 1,
            run_id: "run-1".into(),
            tested_sha: "abc123".into(),
            scenarios: vec![ExpectedScenario {
                id: "scenario-a".into(),
                checks: vec!["check-a".into(), "check-b".into()],
            }],
        }
    }

    fn scenario(checks: &[&str]) -> ScenarioResult {
        ScenarioResult {
            id: "scenario-a".into(),
            status: Status::Passed,
            duration_ms: 1,
            config: json!({}),
            stages: Vec::new(),
            checks: checks
                .iter()
                .map(|id| CheckResult {
                    id: (*id).into(),
                    kind: "test".into(),
                    status: Status::Passed,
                    duration_ms: 1,
                    expected: json!(true),
                    observed: json!(true),
                    message: "passed".into(),
                    next_step: "none".into(),
                    samples: 1,
                    rpc_errors: 0,
                    evidence: Vec::new(),
                })
                .collect(),
            samples: Vec::new(),
            forks: Vec::new(),
            diagnostics: Vec::new(),
            reproduction: String::new(),
        }
    }

    fn run(run_id: &str, checks: &[&str]) -> RunResult {
        RunResult {
            schema_version: 1,
            run_id: run_id.into(),
            tested_sha: "abc123".into(),
            started_at_unix_ms: 1,
            scenarios: vec![scenario(checks)],
        }
    }

    fn write_json(path: &Path, value: &impl Serialize) {
        fs::create_dir_all(path.parent().expect("test path has parent")).unwrap();
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn aggregate(
        temp: &TempDir,
        expected: &ExpectedManifest,
        results: &Path,
        name: &str,
    ) -> RunResult {
        let manifest = temp.path().join(format!("{name}-manifest.json"));
        let output = temp.path().join(format!("{name}-output"));
        write_json(&manifest, expected);
        let result = Aggregate::write(&manifest, results, &output).unwrap();
        assert!(output.join("result.json").is_file());
        assert!(output.join("report.html").is_file());
        assert!(output.join("summary.md").is_file());
        result
    }

    #[test]
    fn missing_stale_duplicate_malformed_and_wrong_checkset_never_pass() {
        let temp = TempDir::new().unwrap();
        let manifest = expected();
        for case in ["missing", "stale", "duplicate", "malformed", "checkset"] {
            let results = temp.path().join(case);
            fs::create_dir_all(&results).unwrap();
            match case {
                "stale" => write_json(
                    &results.join("one/result.json"),
                    &run("old-run", &["check-a", "check-b"]),
                ),
                "duplicate" => {
                    write_json(
                        &results.join("one/result.json"),
                        &run("run-1", &["check-a", "check-b"]),
                    );
                    write_json(
                        &results.join("two/result.json"),
                        &run("run-1", &["check-a", "check-b"]),
                    );
                }
                "malformed" => {
                    fs::create_dir_all(results.join("one")).unwrap();
                    fs::write(results.join("one/result.json"), b"not json").unwrap();
                }
                "checkset" => write_json(
                    &results.join("one/result.json"),
                    &run("run-1", &["check-a", "extra"]),
                ),
                _ => {}
            }
            let result = aggregate(&temp, &manifest, &results, case);
            assert!(!result.passed(), "{case} artifacts must not pass");
            let expected_scenario =
                result.scenarios.iter().find(|scenario| scenario.id == "scenario-a").unwrap();
            assert_eq!(
                expected_scenario
                    .checks
                    .iter()
                    .map(|check| check.id.as_str())
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["check-a", "check-b"]),
            );
        }
    }

    #[test]
    fn invalid_manifest_and_result_discovery_failures_still_write_reports() {
        let temp = TempDir::new().unwrap();
        let bad_manifest = temp.path().join("bad-manifest.json");
        let bad_output = temp.path().join("bad-output");
        fs::write(&bad_manifest, b"{}").unwrap();
        let bad =
            Aggregate::write(&bad_manifest, &temp.path().join("absent"), &bad_output).unwrap();
        assert!(!bad.passed());
        assert!(bad_output.join("report.html").is_file());

        let manifest = expected();
        let missing_results = temp.path().join("missing-results");
        let result = aggregate(&temp, &manifest, &missing_results, "discovery");
        assert!(!result.passed());
        let scenario =
            result.scenarios.iter().find(|scenario| scenario.id == "scenario-a").unwrap();
        assert_eq!(scenario.checks.len(), 2, "infrastructure failures retain expected checks");
    }

    #[test]
    fn evidence_is_relocated_without_collisions_and_missing_evidence_is_an_error() {
        let temp = TempDir::new().unwrap();
        let results = temp.path().join("results");
        let bundle = results.join("one");
        fs::create_dir_all(bundle.join("evidence/logs/a")).unwrap();
        fs::create_dir_all(bundle.join("evidence/logs/b")).unwrap();
        fs::write(bundle.join("evidence/logs/a/out.txt"), "a").unwrap();
        fs::write(bundle.join("evidence/logs/b/out.txt"), "b").unwrap();
        let mut shard = run("run-1", &["check-a", "check-b"]);
        shard.scenarios[0].checks[0].evidence =
            vec!["evidence/logs/a/out.txt".into(), "evidence/logs/b/out.txt".into()];
        shard.scenarios[0].checks[1].evidence = vec!["evidence/logs/missing.txt".into()];
        write_json(&bundle.join("result.json"), &shard);

        let result = aggregate(&temp, &expected(), &results, "evidence");
        assert!(!result.passed());
        let checks = &result.scenarios[0].checks;
        assert_eq!(checks[0].evidence.len(), 2);
        assert_ne!(checks[0].evidence[0], checks[0].evidence[1]);
        let output = temp.path().join("evidence-output");
        assert_eq!(fs::read_to_string(output.join(&checks[0].evidence[0])).unwrap(), "a");
        assert_eq!(fs::read_to_string(output.join(&checks[0].evidence[1])).unwrap(), "b");
        assert_eq!(checks[1].status, Status::Error);
        assert!(checks[1].evidence.is_empty());
        assert!(checks[1].message.contains("evidence/logs/missing.txt"));
        assert!(checks[1].next_step.contains("inspect scenario artifacts"));
    }

    #[test]
    fn shared_evidence_is_copied_once_without_exhausting_the_aggregate_budget() {
        let temp = TempDir::new().unwrap();
        let results = temp.path().join("results");
        let bundle = results.join("one");
        fs::create_dir_all(bundle.join("evidence")).unwrap();
        let log = vec![b'x'; 2 * 1024 * 1024];
        fs::write(bundle.join("evidence/compose.log"), &log).unwrap();
        let ids: Vec<_> = (0..12).map(|i| format!("check-{i}")).collect();
        let names: Vec<_> = ids.iter().map(String::as_str).collect();
        let mut shard = run("run-1", &names);
        for check in &mut shard.scenarios[0].checks {
            check.evidence = vec!["evidence/compose.log".into()];
        }
        write_json(&bundle.join("result.json"), &shard);
        let manifest = ExpectedManifest {
            scenarios: vec![ExpectedScenario { id: "scenario-a".into(), checks: ids }],
            ..expected()
        };
        let result = aggregate(&temp, &manifest, &results, "shared");
        assert!(result.passed(), "{:?}", result.scenarios[0].diagnostics);
        let checks = &result.scenarios[0].checks;
        assert!(checks.iter().all(|check| check.evidence == checks[0].evidence));
        assert_eq!(
            fs::read(temp.path().join("shared-output").join(&checks[0].evidence[0])).unwrap(),
            log
        );
    }

    #[test]
    fn distinct_evidence_still_enforces_the_aggregate_budget() {
        let temp = TempDir::new().unwrap();
        let bundle = temp.path().join("bundle");
        fs::create_dir_all(bundle.join("evidence")).unwrap();
        fs::write(bundle.join("evidence/one.log"), b"a").unwrap();
        fs::write(bundle.join("evidence/two.log"), b"b").unwrap();
        let mut scenario = scenario(&["check-a", "check-b"]);
        scenario.checks[0].evidence = vec!["evidence/one.log".into()];
        scenario.checks[1].evidence = vec!["evidence/two.log".into()];
        let mut copied = MAX_BYTES - 1;
        Aggregate::copy_evidence(
            &bundle.join("result.json"),
            &temp.path().join("output"),
            &mut scenario,
            &mut copied,
        );
        assert_eq!(copied, MAX_BYTES);
        assert_eq!(scenario.checks[0].status, Status::Passed);
        assert_eq!(scenario.checks[1].status, Status::Error);
        assert!(scenario.checks[1].message.contains("aggregate evidence exceeds 20 MiB"));
    }

    #[test]
    fn bounded_read_accepts_the_limit_and_rejects_larger_files() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(20 * 1024 * 1024).unwrap();
        assert_eq!(Aggregate::bounded_read(file.path()).unwrap().len(), 20 * 1024 * 1024);
        file.as_file().set_len(20 * 1024 * 1024 + 1).unwrap();
        assert!(Aggregate::bounded_read(file.path()).unwrap_err().to_string().contains("20 MiB"));
    }

    #[cfg(unix)]
    #[test]
    fn evidence_symlinks_and_escaping_parent_links_are_not_copied() {
        let temp = TempDir::new().unwrap();
        let bundle = temp.path().join("bundle");
        let output = temp.path().join("output");
        let outside = temp.path().join("outside");
        fs::create_dir_all(bundle.join("evidence")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(bundle.join("evidence/regular.txt"), "allowed evidence").unwrap();
        fs::write(outside.join("secret.txt"), "must not be copied").unwrap();
        symlink("regular.txt", bundle.join("evidence/internal-link.txt")).unwrap();
        symlink(outside.join("secret.txt"), bundle.join("evidence/external-link.txt")).unwrap();
        symlink(&outside, bundle.join("evidence/parent-link")).unwrap();
        let mut scenario = scenario(&["check-a"]);
        scenario.checks[0].evidence = vec![
            "evidence/regular.txt".into(),
            "evidence/internal-link.txt".into(),
            "evidence/external-link.txt".into(),
            "evidence/parent-link/secret.txt".into(),
        ];
        let mut copied = 0;
        Aggregate::copy_evidence(&bundle.join("result.json"), &output, &mut scenario, &mut copied);
        assert_eq!(scenario.checks[0].status, Status::Error);
        assert_eq!(scenario.checks[0].evidence.len(), 1);
        assert_eq!(scenario.diagnostics.len(), 3);
        assert_eq!(copied, 16);
        assert_eq!(
            fs::read_to_string(output.join(&scenario.checks[0].evidence[0])).unwrap(),
            "allowed evidence"
        );
        let directory = output.join("evidence/scenario-a/evidence");
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
    }
}
