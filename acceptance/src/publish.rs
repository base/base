use std::{env, fmt::Write as _, path::PathBuf, time::Duration};

use chrono::DateTime;
use clap::Args;
use eyre::{Context, Result, bail, ensure};
use reqwest::{Client, Method, header, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Aggregate, Report, ReportCounts, RunResult, Status};

const MARKER: &str = "<!-- acceptance-results -->";
const META_PREFIX: &str = "<!-- acceptance-results-meta:";
const MAX_PAGES: usize = 20;

/// Trusted workflow inputs for publishing a validated acceptance summary.
#[derive(Debug, Args)]
pub struct PublishArgs {
    /// `GitHub` pull-request event file.
    #[arg(long)]
    pub event: PathBuf,
    /// Untrusted aggregate result artifact.
    #[arg(long)]
    pub result: PathBuf,
    /// Expected scenario manifest artifact.
    #[arg(long)]
    pub expected: PathBuf,
    /// Workflow execution identity, including its attempt.
    #[arg(long)]
    pub run_id: String,
    /// Workflow attempt number.
    #[arg(long)]
    pub attempt: u64,
    /// Trusted workflow start time in YYYY-MM-DDTHH:MM:SSZ format.
    #[arg(long)]
    pub started_at: String,
}

/// Provenance stored inside the marker-owned comment, compatible with earlier publishers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentMetadata {
    /// Execution identity, including attempt.
    pub run_id: String,
    /// Attempt number.
    pub attempt: u64,
    /// Canonical UTC start time, sortable lexicographically.
    pub started_at: String,
}

impl CommentMetadata {
    /// Rejects invalid ordering information before any publication can occur.
    pub fn validate(&self) -> Result<()> {
        let date = DateTime::parse_from_rfc3339(&self.started_at)
            .wrap_err("started-at must be a UTC timestamp in YYYY-MM-DDTHH:MM:SSZ format")?;
        ensure!(
            self.started_at.len() == 20
                && date.format("%Y-%m-%dT%H:%M:%SZ").to_string() == self.started_at,
            "started-at must be a UTC timestamp in YYYY-MM-DDTHH:MM:SSZ format"
        );
        ensure!(self.attempt > 0, "attempt must be positive");
        ensure!(
            !self.run_id.is_empty()
                && self.run_id.len() <= 200
                && self.run_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b)),
            "invalid run identity"
        );
        Ok(())
    }

    /// Reads only the exact metadata prefix of an owned comment.
    pub fn parse(body: &str) -> Result<Self> {
        let encoded = body
            .strip_prefix(&format!("{MARKER}\n{META_PREFIX}"))
            .and_then(|tail| tail.split_once(" -->").map(|(metadata, _)| metadata))
            .ok_or_else(|| eyre::eyre!("owned marker has invalid metadata"))?;
        let metadata: Self = serde_json::from_str(encoded)?;
        metadata.validate()?;
        Ok(metadata)
    }
}

/// `GitHub` comment publisher with bounded requests and narrow comment ownership.
#[derive(Debug)]
pub struct PrPublisher {
    /// Authenticated client; authorization headers must be marked sensitive.
    pub client: Client,
    /// Repository API URL (production always uses api.github.com).
    pub api_url: String,
    /// Pull-request number.
    pub pr: u64,
    /// Expected pull-request head revision.
    pub head: String,
    /// Expected pull-request base revision.
    pub base: String,
    /// Exact bot login allowed to own the comment.
    pub bot_login: String,
}

impl PublishArgs {
    /// Publishes from trusted CI environment and artifact data, never executing artifact code.
    pub async fn execute(self) -> Result<()> {
        let metadata = CommentMetadata {
            run_id: self.run_id.clone(),
            attempt: self.attempt,
            started_at: self.started_at.clone(),
        };
        metadata.validate()?;
        let repo = env::var("GITHUB_REPOSITORY")?;
        let parts: Vec<_> = repo.split('/').collect();
        ensure!(
            parts.len() == 2
                && parts.iter().all(|part| !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
            "invalid repository"
        );
        let tested_sha = env::var("TESTED_SHA")?;
        PrPublisher::validate_sha(&tested_sha)?;
        let event: Value = serde_json::from_slice(&Aggregate::bounded_read(&self.event)?)?;
        let pr = event["pull_request"]["number"]
            .as_u64()
            .filter(|number| *number > 0)
            .ok_or_else(|| eyre::eyre!("missing PR number"))?;
        let head = event["pull_request"]["head"]["sha"].as_str().unwrap_or_default().to_owned();
        let base = event["pull_request"]["base"]["sha"].as_str().unwrap_or_default().to_owned();
        PrPublisher::validate_sha(&head)?;
        PrPublisher::validate_sha(&base)?;
        let run = self.load_result(&tested_sha).unwrap_or_else(|_| {
            let mut run = Aggregate::fallback(
                "Missing or invalid acceptance artifacts; inspect PR checks and report artifacts."
                    .into(),
            );
            run.tested_sha.clone_from(&tested_sha);
            run.run_id.clone_from(&self.run_id);
            run
        });
        let body = PrPublisher::body(
            &run,
            &format!("https://github.com/{repo}/pull/{pr}/checks"),
            &metadata,
        )?;
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {}", env::var("GITHUB_TOKEN")?))?;
        authorization.set_sensitive(true);
        let publisher = PrPublisher {
            client: Client::builder()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(20))
                .user_agent("base-acceptance")
                .default_headers(header::HeaderMap::from_iter([
                    (header::AUTHORIZATION, authorization),
                    (
                        header::ACCEPT,
                        header::HeaderValue::from_static("application/vnd.github+json"),
                    ),
                    (
                        header::HeaderName::from_static("x-github-api-version"),
                        header::HeaderValue::from_static("2022-11-28"),
                    ),
                ]))
                .build()?,
            api_url: format!("https://api.github.com/repos/{repo}"),
            pr,
            head,
            base,
            bot_login: env::var("BOT_LOGIN")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "depot-code-access[bot]".into()),
        };
        println!(
            "{}",
            if publisher.publish(&metadata, &body).await? {
                "published"
            } else {
                "stale run; publication skipped"
            }
        );
        Ok(())
    }

    /// Reuses portable schema validation and enforces exact artifact provenance and check sets.
    pub fn load_result(&self, tested_sha: &str) -> Result<RunResult> {
        PrPublisher::validate_sha(tested_sha)?;
        let expected = Aggregate::load_expected(&self.expected)?;
        let run: RunResult = serde_json::from_slice(&Aggregate::bounded_read(&self.result)?)?;
        Report::validate(&run)?;
        ensure!(
            expected.run_id == self.run_id
                && expected.tested_sha == tested_sha
                && run.run_id == self.run_id
                && run.tested_sha == tested_sha,
            "artifact provenance mismatch"
        );
        ensure!(expected.scenarios.len() == run.scenarios.len(), "scenario set mismatch");
        for (wanted, actual) in expected.scenarios.iter().zip(&run.scenarios) {
            ensure!(
                wanted.id == actual.id
                    && wanted
                        .checks
                        .iter()
                        .map(String::as_str)
                        .eq(actual.checks.iter().map(|check| check.id.as_str())),
                "scenario/check set mismatch"
            );
        }
        Ok(run)
    }
}

impl PrPublisher {
    /// Validates a full `GitHub` commit identity.
    pub fn validate_sha(sha: &str) -> Result<()> {
        ensure!(
            sha.len() == 40
                && sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid tested revision"
        );
        Ok(())
    }

    /// Performs one bounded `GitHub` request without printing credentials or response bodies.
    pub async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut request = self.client.request(method, format!("{}{path}", self.api_url));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.wrap_err("GitHub request failed")?;
        ensure!(response.status().is_success(), "GitHub HTTP status {}", response.status());
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= 20 * 1024 * 1024,
                "GitHub response exceeds 20 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).wrap_err("invalid GitHub JSON response")
    }

    /// Rechecks both branch identities, including immediately before a write.
    pub async fn current(&self) -> Result<bool> {
        let item = self.request(Method::GET, &format!("/pulls/{}", self.pr), None).await?;
        Ok(item["head"]["sha"].as_str() == Some(&self.head)
            && item["base"]["sha"].as_str() == Some(&self.base))
    }

    /// Upserts exactly one marker-owned bot comment, refusing stale or ambiguous writes.
    pub async fn publish(&self, metadata: &CommentMetadata, body: &str) -> Result<bool> {
        metadata.validate()?;
        Report::bounded("comment body", body, 60_000)?;
        ensure!(body.starts_with(MARKER), "missing comment marker");
        if !self.current().await? {
            return Ok(false);
        }
        let mut owned = None;
        for page in 1..=MAX_PAGES {
            let response = self
                .request(
                    Method::GET,
                    &format!("/issues/{}/comments?per_page=100&page={page}", self.pr),
                    None,
                )
                .await?;
            let comments =
                response.as_array().ok_or_else(|| eyre::eyre!("invalid comment page"))?;
            ensure!(comments.len() <= 100, "oversized comment page");
            for comment in comments {
                if comment["user"]["type"] == "Bot"
                    && comment["user"]["login"].as_str() == Some(&self.bot_login)
                    && comment["body"].as_str().is_some_and(|body| body.starts_with(MARKER))
                {
                    ensure!(owned.is_none(), "duplicate owned comments");
                    owned = Some(comment.clone());
                }
            }
            if comments.len() < 100 {
                break;
            }
            ensure!(page < MAX_PAGES, "comment pagination cap reached");
        }
        if let Some(comment) = &owned {
            let old = CommentMetadata::parse(comment["body"].as_str().unwrap_or_default())?;
            if (old.run_id == metadata.run_id && old.attempt > metadata.attempt)
                || (old.run_id != metadata.run_id && old.started_at >= metadata.started_at)
            {
                return Ok(false);
            }
        }
        let (method, path) = if let Some(comment) = owned {
            let id = comment["id"]
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or_else(|| eyre::eyre!("invalid owned comment id"))?;
            (Method::PATCH, format!("/issues/comments/{id}"))
        } else {
            (Method::POST, format!("/issues/{}/comments", self.pr))
        };
        if !self.current().await? {
            return Ok(false);
        }
        self.request(method, &path, Some(json!({"body": body}))).await?;
        Ok(true)
    }

    /// Escapes untrusted content for both Markdown and embedded HTML summary text.
    pub fn text(value: &str) -> String {
        let mut out = String::new();
        for word in value.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            if word.to_ascii_lowercase().contains("http://")
                || word.to_ascii_lowercase().contains("https://")
            {
                out.push_str("[link removed]");
                continue;
            }
            out.push_str(word);
        }
        let out: String = out.chars().take(2_000).collect();
        out.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('@', "＠")
            .replace('`', "&#96;")
            .replace('|', "&#124;")
            .replace('[', "&#91;")
            .replace(']', "&#93;")
            .replace('(', "&#40;")
            .replace(')', "&#41;")
    }

    /// Builds bounded collapsible results with record-derived verdicts and lifecycle failures.
    pub fn body(run: &RunResult, checks_url: &str, metadata: &CommentMetadata) -> Result<String> {
        Report::validate(run)?;
        metadata.validate()?;
        let counts = ReportCounts::from_run(run);
        let mut out = format!(
            "{MARKER}\n{META_PREFIX}{} -->\n## Acceptance: {}\n\nRevision `{}` · [PR checks]({checks_url})\n\n| Passed | Failed | Error | Blocked | Cancelled |\n|---:|---:|---:|---:|---:|\n| {} | {} | {} | {} | {} |\n\n",
            serde_json::to_string(metadata)?,
            if run.passed() { "✅ Passed" } else { "❌ Not passed" },
            Self::text(&run.tested_sha),
            counts.passed,
            counts.failed,
            counts.error,
            counts.blocked,
            counts.cancelled
        );
        for scenario in &run.scenarios {
            write!(
                out,
                "<details{}><summary>{} — {}</summary>\n\n",
                if scenario.outcome() == Status::Passed { "" } else { " open" },
                Self::text(&scenario.id),
                scenario.outcome().label()
            )?;
            for stage in scenario.stages.iter().filter(|stage| stage.status != Status::Passed) {
                write!(
                    out,
                    "**Lifecycle: {} — {}**: {}\n\n",
                    Self::text(&stage.id),
                    stage.status.label(),
                    Self::text(&stage.message)
                )?;
            }
            for check in &scenario.checks {
                write!(
                    out,
                    "**{}** — {}\n\n- Expected: `{}`\n- Observed: `{}`\n- Message: {}\n- Next step: {}\n\n",
                    Self::text(&check.id),
                    check.status.label(),
                    Self::text(&Report::compact_json(&check.expected)),
                    Self::text(&Report::compact_json(&check.observed)),
                    Self::text(&check.message),
                    Self::text(&check.next_step)
                )?;
            }
            write!(
                out,
                "Reproduction: `{}`\n\n</details>\n\n",
                Self::text(&scenario.reproduction)
            )?;
            if out.len() > 60_000 {
                bail!("comment body exceeds bound");
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    //! HTTP fixtures exercise reqwest itself, including ordered reads before a write;
    //! no internal trait is replaced by a hand-written mock.

    use std::fs;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;
    use crate::{ExpectedManifest, ExpectedScenario};

    fn metadata() -> CommentMetadata {
        CommentMetadata {
            run_id: "run/attempt-2".into(),
            attempt: 2,
            started_at: "2026-01-02T00:00:00Z".into(),
        }
    }

    fn run() -> RunResult {
        let mut run: RunResult =
            serde_json::from_str(include_str!("../fixtures/reports/synthetic-mixed.json")).unwrap();
        run.run_id = metadata().run_id;
        run.tested_sha = "a".repeat(40);
        run
    }

    fn current() -> Value {
        json!({"head":{"sha":"head"},"base":{"sha":"base"}})
    }

    fn owned(metadata: &CommentMetadata) -> Value {
        json!({"id":777,"user":{"login":"depot-code-access[bot]","type":"Bot"},
            "body": format!("{MARKER}\n{META_PREFIX}{} -->\nprior result", serde_json::to_string(metadata).unwrap())})
    }

    async fn server(steps: Vec<(String, Value)>) -> (PrPublisher, JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for (expected, reply) in steps {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (end, length) = loop {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        assert_eq!(headers.lines().next().unwrap(), format!("{expected} HTTP/1.1"));
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while bytes.len() < end + length {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buffer[..read]);
                }
                if length > 0 {
                    bodies.push(serde_json::from_slice(&bytes[end..end + length]).unwrap());
                }
                let body = reply.to_string();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            bodies
        });
        (
            PrPublisher {
                client: Client::builder().timeout(Duration::from_secs(2)).build().unwrap(),
                api_url: format!("http://{address}"),
                pr: 1,
                head: "head".into(),
                base: "base".into(),
                bot_login: "depot-code-access[bot]".into(),
            },
            handle,
        )
    }

    #[test]
    fn timestamp_and_metadata_validation_preserve_trusted_ordering() {
        metadata().validate().unwrap();
        for invalid in [
            "",
            "invalid",
            "2026-02-30T00:00:00Z",
            "2026-01-02T00:00:00",
            "2026-1-2T00:00:00Z",
            "2026-01-02T01:00:00+01:00",
            "2026-01-02T00:00:00.123Z",
        ] {
            assert!(
                CommentMetadata { started_at: invalid.into(), ..metadata() }.validate().is_err(),
                "{invalid}"
            );
        }
        assert!(CommentMetadata { run_id: "-->@team".into(), ..metadata() }.validate().is_err());
        assert!(CommentMetadata { attempt: 0, ..metadata() }.validate().is_err());
        assert!(CommentMetadata::parse(MARKER).is_err());
        let parsed = CommentMetadata::parse(owned(&metadata())["body"].as_str().unwrap()).unwrap();
        assert_eq!(parsed.started_at, "2026-01-02T00:00:00Z");
    }

    #[tokio::test]
    async fn invalid_timestamp_is_rejected_before_environment_or_artifact_access() {
        let args = PublishArgs {
            event: "missing".into(),
            result: "missing".into(),
            expected: "missing".into(),
            run_id: "run".into(),
            attempt: 1,
            started_at: "invalid".into(),
        };
        assert!(args.execute().await.unwrap_err().to_string().contains("started-at"));
    }

    #[test]
    fn artifact_schema_provenance_and_exact_check_sets_are_required() {
        let directory = tempfile::tempdir().unwrap();
        let original = run();
        let args = PublishArgs {
            event: PathBuf::new(),
            result: directory.path().join("result.json"),
            expected: directory.path().join("expected.json"),
            run_id: original.run_id.clone(),
            attempt: 2,
            started_at: metadata().started_at,
        };
        let expected = ExpectedManifest {
            schema_version: 1,
            run_id: original.run_id.clone(),
            tested_sha: original.tested_sha.clone(),
            scenarios: original
                .scenarios
                .iter()
                .map(|scenario| ExpectedScenario {
                    id: scenario.id.clone(),
                    checks: scenario.checks.iter().map(|check| check.id.clone()).collect(),
                })
                .collect(),
        };
        fs::write(&args.expected, serde_json::to_vec(&expected).unwrap()).unwrap();
        assert!(args.load_result(&original.tested_sha).is_err());
        fs::write(&args.result, serde_json::to_vec(&original).unwrap()).unwrap();
        assert!(!args.load_result(&original.tested_sha).unwrap().passed());
        let mut variants = Vec::new();
        let mut stale = original.clone();
        stale.run_id = "other".into();
        variants.push(stale);
        let mut stale = original.clone();
        stale.tested_sha = "b".repeat(40);
        variants.push(stale);
        let mut missing = original.clone();
        missing.scenarios.clear();
        variants.push(missing);
        let mut duplicate = original.clone();
        duplicate.scenarios.push(original.scenarios[0].clone());
        variants.push(duplicate);
        let mut wrong_check = original.clone();
        wrong_check.scenarios[0].checks[0].id = "unexpected".into();
        variants.push(wrong_check);
        let mut bad_path = original.clone();
        bad_path.scenarios[0].checks[0].evidence = vec!["../secret".into()];
        variants.push(bad_path);
        for invalid in variants {
            fs::write(&args.result, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(args.load_result(&original.tested_sha).is_err());
        }
        fs::write(&args.result, b"not json").unwrap();
        assert!(args.load_result(&original.tested_sha).is_err());
    }

    #[test]
    fn comments_preserve_outcomes_escape_content_and_explain_cleanup_errors() {
        let mut run = run();
        let body =
            PrPublisher::body(&run, "https://github.com/base/base/pull/1/checks", &metadata())
                .unwrap();
        assert!(body.contains("| 1 | 1 | 0 | 2 | 0 |"));
        assert!(body.contains("<details><summary>healthy-synthetic — passed"));
        assert!(body.contains("<details open><summary>lag-failure-synthetic — failed"));
        assert!(body.contains("- Expected: `{\"chain_id\":84538453}`"));
        assert!(
            body.contains("Expected:") && body.contains("Observed:") && body.contains("Next step:")
        );
        run.scenarios.truncate(1);
        run.scenarios[0].checks[0].message =
            "<script>|`x`\n@team [click](https://evil.invalid/x)".into();
        run.scenarios[0].stages.last_mut().unwrap().status = Status::Error;
        run.scenarios[0].stages.last_mut().unwrap().message = "owned cleanup failed".into();
        run.scenarios[0].samples = vec![run.scenarios[0].samples[0].clone(); 600];
        let body =
            PrPublisher::body(&run, "https://github.com/base/base/pull/1/checks", &metadata())
                .unwrap();
        assert!(body.contains("Not passed") && body.contains("owned cleanup failed"));
        assert!(body.contains("&lt;script&gt;") && body.contains("＠team"));
        assert!(
            !body.contains("<script>") && !body.contains("@team") && !body.contains("https://evil")
        );
        let check = run.scenarios[0].checks[0].clone();
        run.scenarios[0].checks = (0..200)
            .map(|id| {
                let mut check = check.clone();
                check.id = format!("check-{id}");
                check.message = "x".repeat(2_000);
                check
            })
            .collect();
        assert!(
            PrPublisher::body(&run, "https://github.com/base/base/pull/1/checks", &metadata())
                .is_err()
        );
        let error = Aggregate::fallback("missing result".into());
        assert!(
            PrPublisher::body(&error, "https://github.com/base/base/pull/1/checks", &metadata())
                .unwrap()
                .contains("Not passed")
        );
    }

    #[tokio::test]
    async fn stale_head_or_base_never_writes_even_if_changed_during_pagination() {
        for changed in [
            json!({"head":{"sha":"new"},"base":{"sha":"base"}}),
            json!({"head":{"sha":"head"},"base":{"sha":"new"}}),
        ] {
            for changed_late in [false, true] {
                let mut steps = Vec::new();
                if changed_late {
                    steps.push(("GET /pulls/1".into(), current()));
                    steps.push(("GET /issues/1/comments?per_page=100&page=1".into(), json!([])));
                }
                steps.push(("GET /pulls/1".into(), changed.clone()));
                let (publisher, server) = server(steps).await;
                assert!(!publisher.publish(&metadata(), MARKER).await.unwrap());
                assert!(server.await.unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn newer_runs_and_attempts_cannot_be_overwritten() {
        for old in [
            CommentMetadata { attempt: 3, ..metadata() },
            CommentMetadata {
                run_id: "other".into(),
                started_at: "2026-01-03T00:00:00Z".into(),
                ..metadata()
            },
        ] {
            let (publisher, server) = server(vec![
                ("GET /pulls/1".into(), current()),
                ("GET /issues/1/comments?per_page=100&page=1".into(), json!([owned(&old)])),
            ])
            .await;
            assert!(!publisher.publish(&metadata(), MARKER).await.unwrap());
            assert!(server.await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn paginated_update_only_modifies_the_exact_owned_bot_comment() {
        let other = json!({"user":{"login":"other[bot]","type":"Bot"},"body":MARKER});
        let human = json!({"user":{"login":"depot-code-access[bot]","type":"User"},"body":MARKER});
        let unrelated =
            json!({"user":{"login":"depot-code-access[bot]","type":"Bot"},"body":"other results"});
        let (publisher, server) = server(vec![
            ("GET /pulls/1".into(), current()),
            ("GET /issues/1/comments?per_page=100&page=1".into(), json!(vec![other; 100])),
            (
                "GET /issues/1/comments?per_page=100&page=2".into(),
                json!([human, unrelated, owned(&metadata())]),
            ),
            ("GET /pulls/1".into(), current()),
            ("PATCH /issues/comments/777".into(), json!({})),
        ])
        .await;
        let body =
            PrPublisher::body(&run(), "https://github.com/base/base/pull/1/checks", &metadata())
                .unwrap();
        assert!(publisher.publish(&metadata(), &body).await.unwrap());
        assert_eq!(server.await.unwrap(), vec![json!({"body":body})]);
    }

    #[tokio::test]
    async fn fresh_comment_is_created_and_ambiguous_comments_are_rejected() {
        let (publisher, server) = server(vec![
            ("GET /pulls/1".into(), current()),
            ("GET /issues/1/comments?per_page=100&page=1".into(), json!([])),
            ("GET /pulls/1".into(), current()),
            ("POST /issues/1/comments".into(), json!({})),
        ])
        .await;
        assert!(publisher.publish(&metadata(), MARKER).await.unwrap());
        assert_eq!(server.await.unwrap(), vec![json!({"body":MARKER})]);
        for page in [
            json!([owned(&metadata()), owned(&metadata())]),
            json!([{"id":7,"user":{"type":"Bot","login":"depot-code-access[bot]"},"body":MARKER}]),
        ] {
            let (publisher, task) = self::server(vec![
                ("GET /pulls/1".into(), current()),
                ("GET /issues/1/comments?per_page=100&page=1".into(), page),
            ])
            .await;
            assert!(publisher.publish(&metadata(), MARKER).await.is_err());
            assert!(task.await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn pagination_cap_refuses_to_duplicate_an_unseen_comment() {
        let mut steps = vec![("GET /pulls/1".into(), current())];
        for page in 1..=MAX_PAGES {
            steps.push((
                format!("GET /issues/1/comments?per_page=100&page={page}"),
                json!(vec![json!({"user":{"type":"User"}}); 100]),
            ));
        }
        let (publisher, server) = server(steps).await;
        assert!(
            publisher
                .publish(&metadata(), MARKER)
                .await
                .unwrap_err()
                .to_string()
                .contains("pagination cap")
        );
        assert!(server.await.unwrap().is_empty());
    }
}
