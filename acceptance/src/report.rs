use std::{
    collections::HashSet,
    fmt::Write as _,
    fs,
    path::{Component, Path},
};

use eyre::{Result, WrapErr, bail};
use serde_json::Value;

use crate::{RunResult, ScenarioResult, Status};

/// Deterministic renderers for a validated acceptance result.
#[derive(Debug)]
pub struct Report;

/// Counts derived exclusively from check records.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReportCounts {
    /// Passing checks.
    pub passed: usize,
    /// Failed checks.
    pub failed: usize,
    /// Checks which could not be evaluated because of an error.
    pub error: usize,
    /// Checks blocked by a prerequisite.
    pub blocked: usize,
    /// Cancelled checks.
    pub cancelled: usize,
}

impl ReportCounts {
    /// Derives counters from records rather than supplied aggregate statuses.
    pub fn from_run(run: &RunResult) -> Self {
        let mut counts = Self::default();
        for check in run.scenarios.iter().flat_map(|scenario| &scenario.checks) {
            match check.status {
                Status::Passed => counts.passed += 1,
                Status::Failed => counts.failed += 1,
                Status::Error => counts.error += 1,
                Status::Blocked => counts.blocked += 1,
                Status::Cancelled => counts.cancelled += 1,
            }
        }
        counts
    }

    /// Returns the failure-first verdict derived from scenario records.
    pub fn verdict(run: &RunResult) -> Status {
        let outcomes: Vec<_> = run.scenarios.iter().map(ScenarioResult::outcome).collect();
        for status in [Status::Error, Status::Cancelled, Status::Failed, Status::Blocked] {
            if outcomes.contains(&status) {
                return status;
            }
        }
        Status::Passed
    }

    /// Returns a phrase distinguishing check failures from infrastructure failures.
    pub fn status_phrase(run: &RunResult) -> &'static str {
        let has_infra = run.scenarios.iter().any(|scenario| {
            matches!(scenario.outcome(), Status::Error | Status::Cancelled | Status::Blocked)
        });
        let has_failure = run
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.checks)
            .any(|check| check.status == Status::Failed);
        match (has_failure, has_infra) {
            (true, true) => "FAILED + INFRASTRUCTURE ERROR",
            (true, false) => "FAILED",
            (false, true) => "INCOMPLETE / INFRASTRUCTURE ERROR",
            _ => "PASSED",
        }
    }
}

impl Report {
    /// Renders a self-contained, offline HTML report.
    pub fn html(run: &RunResult) -> Result<String> {
        Self::validate(run)?;
        let counts = ReportCounts::from_run(run);
        let mut scenarios: Vec<_> = run.scenarios.iter().collect();
        scenarios.sort_by_key(|scenario| (scenario.outcome() == Status::Passed, &scenario.id));
        let verdict = ReportCounts::verdict(run);
        let mut out = String::with_capacity(32_000);
        write!(
            out,
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\"><title>Acceptance report — {}</title><style>{}</style></head><body><main>",
            Self::escape_html(&run.run_id),
            include_str!("../report/style.css")
        )?;
        write!(
            out,
            "<header class=\"hero\"><div class=\"eyebrow\">BASE ACCEPTANCE · schema {}</div><h1>Acceptance report</h1><div class=\"verdict {}\">{}</div><p>Run <strong>{}</strong> · revision <code>{}</code> · started {} ms Unix</p><div class=\"counters\">",
            run.schema_version,
            verdict.label(),
            ReportCounts::status_phrase(run),
            Self::escape_html(&run.run_id),
            Self::escape_html(&run.tested_sha),
            run.started_at_unix_ms
        )?;
        for (label, value, class) in [
            ("Passed", counts.passed, "passed"),
            ("Failed", counts.failed, "failed"),
            ("Error", counts.error, "error"),
            ("Blocked", counts.blocked, "blocked"),
            ("Cancelled", counts.cancelled, "cancelled"),
        ] {
            write!(
                out,
                "<div class=\"counter\"><span>{label}</span><strong class=\"{class}\">{value}</strong></div>"
            )?;
        }
        out.push_str("</div></header><section class=\"card\"><h2>Scenario lifecycle</h2><div class=\"scroll\"><table><thead><tr><th>Scenario</th><th>Setup</th><th>Readiness</th><th>Checks</th><th>Cleanup</th><th>Result</th></tr></thead><tbody>");
        for scenario in &scenarios {
            write!(
                out,
                "<tr><th><a href=\"#scenario-{}\">{}</a></th>",
                Self::anchor(&scenario.id),
                Self::escape_html(&scenario.id)
            )?;
            for stage in ["setup", "readiness", "checks", "cleanup"] {
                out.push_str(&Self::stage_cell(scenario, stage));
            }
            out.push_str(&Self::status_html(scenario.outcome()));
            out.push_str("</tr>");
        }
        out.push_str("</tbody></table></div></section>");
        for scenario in scenarios {
            Self::render_scenario(&mut out, scenario)?;
        }
        out.push_str("</main></body></html>");
        Ok(out)
    }

    /// Renders the bounded marker-owned Markdown summary used for PR comments.
    pub fn markdown(run: &RunResult) -> Result<String> {
        Self::validate(run)?;
        const LIMIT: usize = 50 * 1024;
        const RESERVE: usize = 160;
        let counts = ReportCounts::from_run(run);
        let mut out = format!(
            "<!-- acceptance-results -->\n## Acceptance: {}\n\nRun `{}` · revision `{}`\n\n| Passed | Failed | Error | Blocked | Cancelled |\n|---:|---:|---:|---:|---:|\n| {} | {} | {} | {} | {} |\n\n",
            Self::markdown_text(ReportCounts::status_phrase(run)),
            Self::markdown_text(&run.run_id),
            Self::markdown_text(&run.tested_sha),
            counts.passed,
            counts.failed,
            counts.error,
            counts.blocked,
            counts.cancelled
        );
        let total = run.scenarios.iter().map(|s| s.checks.len()).sum::<usize>();
        let mut shown = 0;
        for scenario in &run.scenarios {
            let mut section = format!(
                "<details{}><summary>{} — {}</summary>\n\n",
                if scenario.outcome() == Status::Passed { "" } else { " open" },
                Self::markdown_text(&scenario.id),
                scenario.outcome().label()
            );
            for check in &scenario.checks {
                let line = format!(
                    "- **{}** (`{}`): {} — expected `{}`, observed `{}`. {}\n  - Next step: {}\n  - Reproduce: `{}`\n  - Evidence: {}\n",
                    Self::markdown_text(&check.id),
                    Self::markdown_text(&check.kind),
                    check.status.label(),
                    Self::markdown_text(&Self::compact_json(&check.expected)),
                    Self::markdown_text(&Self::compact_json(&check.observed)),
                    Self::markdown_text(&check.message),
                    Self::markdown_text(&check.next_step),
                    Self::markdown_text(&scenario.reproduction),
                    if check.evidence.is_empty() {
                        "⚠ none recorded".into()
                    } else {
                        check
                            .evidence
                            .iter()
                            .map(|path| format!("`{}`", Self::markdown_text(path)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
                if out.len() + section.len() + line.len() + RESERVE > LIMIT {
                    break;
                }
                section.push_str(&line);
                shown += 1;
            }
            section.push_str("\n</details>\n\n");
            if out.len() + section.len() + RESERVE > LIMIT {
                break;
            }
            out.push_str(&section);
        }
        if shown < total {
            writeln!(
                out,
                "_{} check(s) omitted to keep this comment below 50 KiB; download the report bundle for complete results._",
                total - shown
            )?;
        }
        Ok(out)
    }

    /// Writes the portable report bundle, leaving an existing evidence directory intact.
    pub fn write(run: &RunResult, directory: &Path) -> Result<()> {
        Self::validate(run)?;
        fs::create_dir_all(directory).wrap_err("create report directory")?;
        Self::validate_existing_evidence(run, directory)?;
        for name in ["result.json", "report.html", "summary.md"] {
            let destination = directory.join(name);
            if destination
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                bail!("refusing to overwrite symlink report file {name}");
            }
        }
        let json = serde_json::to_string_pretty(run).wrap_err("serialize result")?;
        let temporary = directory.join(format!(".result.json.{}.tmp", std::process::id()));
        if temporary.symlink_metadata().is_ok() {
            bail!("temporary result path already exists");
        }
        fs::write(&temporary, format!("{json}\n")).wrap_err("write temporary result.json")?;
        fs::rename(&temporary, directory.join("result.json")).wrap_err("commit result.json")?;
        fs::write(directory.join("report.html"), Self::html(run)?).wrap_err("write report.html")?;
        fs::write(directory.join("summary.md"), Self::markdown(run)?)
            .wrap_err("write summary.md")?;
        Ok(())
    }

    /// Validates reporting schema, resource bounds, identifiers, and artifact paths.
    pub fn validate(run: &RunResult) -> Result<()> {
        if run.schema_version != 1 {
            bail!("unsupported result schema {}", run.schema_version);
        }
        Self::bounded("run_id", &run.run_id, 200)?;
        Self::bounded("tested_sha", &run.tested_sha, 200)?;
        if run.scenarios.is_empty() || run.scenarios.len() > 100 {
            bail!("result must contain 1..=100 scenarios");
        }
        let mut scenario_ids = HashSet::new();
        let mut total_checks = 0usize;
        let mut total_samples = 0usize;
        for scenario in &run.scenarios {
            Self::valid_id("scenario", &scenario.id)?;
            if !scenario_ids.insert(&scenario.id) {
                bail!("duplicate scenario id {}", scenario.id);
            }
            if scenario.checks.len() > 1_000 || scenario.stages.len() > 100 {
                bail!("scenario {} exceeds record bounds", scenario.id);
            }
            total_checks += scenario.checks.len();
            total_samples += scenario.samples.len();
            if total_checks > 5_000 || total_samples > 50_000 {
                bail!("result exceeds reporting bounds");
            }
            let mut stage_ids = HashSet::new();
            for stage in &scenario.stages {
                Self::valid_id("stage", &stage.id)?;
                Self::bounded("stage message", &stage.message, 65_536)?;
                Self::reject_secret("stage message", &stage.message)?;
                if !stage_ids.insert(&stage.id) {
                    bail!("duplicate stage id {}", stage.id);
                }
            }
            let mut check_ids = HashSet::new();
            for check in &scenario.checks {
                Self::valid_id("check", &check.id)?;
                Self::bounded("check kind", &check.kind, 100)?;
                for (name, value) in [("message", &check.message), ("next_step", &check.next_step)]
                {
                    Self::bounded(name, value, 65_536)?;
                    Self::reject_secret(name, value)?;
                }
                if !check_ids.insert(&check.id) {
                    bail!("duplicate check id {}", check.id);
                }
                for path in &check.evidence {
                    Self::validate_evidence_path(path)?;
                }
                Self::validate_value(&check.expected, 0)?;
                Self::validate_value(&check.observed, 0)?;
            }
            Self::validate_value(&scenario.config, 0)?;
            Self::bounded("reproduction", &scenario.reproduction, 4_096)?;
            Self::reject_secret("reproduction", &scenario.reproduction)?;
            for diagnostic in &scenario.diagnostics {
                Self::bounded("diagnostic", diagnostic, 65_536)?;
                Self::reject_secret("diagnostic", diagnostic)?;
            }
            for fork in &scenario.forks {
                Self::bounded("fork name", &fork.name, 200)?;
                Self::bounded("fork chain", &fork.chain, 100)?;
                if fork.observed_block.is_some() != fork.observed_elapsed_ms.is_some() {
                    bail!("fork {} has a partial observed boundary", fork.name);
                }
            }
            for sample in &scenario.samples {
                Self::bounded("endpoint", &sample.endpoint, 100)?;
                if let Some(hash) = &sample.hash {
                    Self::bounded("sample hash", hash, 200)?;
                }
                if sample.number.is_none() && (sample.timestamp.is_some() || sample.hash.is_some())
                {
                    bail!("gap sample for {} contains block data", sample.endpoint);
                }
            }
        }
        Ok(())
    }
    /// Appends one semantic scenario section to an HTML document.
    pub fn render_scenario(out: &mut String, scenario: &ScenarioResult) -> Result<()> {
        let open = scenario.outcome() != Status::Passed;
        write!(
            out,
            "<details class=\"scenario\" id=\"scenario-{}\"{}><summary>{} — {}</summary><p>{} ms · recorded aggregate: {} · derived result: {}</p>",
            Self::anchor(&scenario.id),
            if open { " open" } else { "" },
            Self::escape_html(&scenario.id),
            scenario.outcome().label(),
            scenario.duration_ms,
            scenario.status.label(),
            scenario.outcome().label()
        )?;
        for check in &scenario.checks {
            write!(
                out,
                "<article class=\"card\" id=\"check-{}-{}\"><h3>{} <span class=\"pill {}\">{}</span></h3><p>{}</p><div class=\"grid\"><div><strong>Expected</strong><div class=\"kv\">{}</div></div><div><strong>Observed</strong><div class=\"kv\">{}</div></div></div><p><strong>Next step:</strong> {}</p><p class=\"muted\">{} samples · {} RPC errors · {} ms</p>",
                Self::anchor(&scenario.id),
                Self::anchor(&check.id),
                Self::escape_html(&check.id),
                check.status.label(),
                check.status.label(),
                Self::escape_html(&check.message),
                Self::escape_html(&Self::pretty_json(&check.expected)),
                Self::escape_html(&Self::pretty_json(&check.observed)),
                Self::escape_html(&check.next_step),
                check.samples,
                check.rpc_errors,
                check.duration_ms
            )?;
            if !check.evidence.is_empty() {
                out.push_str("<p class=\"evidence\"><strong>Evidence:</strong> ");
                for (i, path) in check.evidence.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write!(
                        out,
                        "<a href=\"{}\">{}</a>",
                        Self::escape_html(path),
                        Self::escape_html(path)
                    )?;
                }
                out.push_str("</p>");
            } else {
                out.push_str(
                    "<p class=\"evidence warning\"><strong>Evidence:</strong> none recorded</p>",
                );
            }
            out.push_str("</article>");
        }
        if !scenario.samples.is_empty() {
            out.push_str(&Self::head_chart(scenario)?);
        }
        out.push_str("<details><summary>Accessible head sample data</summary><div class=\"scroll\"><table class=\"chart-table\"><thead><tr><th>Endpoint</th><th>Elapsed ms</th><th>Height</th><th>Timestamp</th><th>Hash</th></tr></thead><tbody>");
        for sample in &scenario.samples {
            write!(
                out,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td></tr>",
                Self::escape_html(&sample.endpoint),
                sample.elapsed_ms,
                Self::option_u64(sample.number),
                Self::option_u64(sample.timestamp),
                Self::escape_html(sample.hash.as_deref().unwrap_or("gap"))
            )?;
        }
        out.push_str("</tbody></table></div></details>");
        write!(
            out,
            "<details><summary>Configuration, provenance, diagnostics, and reproduction</summary><h3>Resolved configuration</h3><div class=\"kv\">{}</div>",
            Self::escape_html(&Self::pretty_json(&scenario.config))
        )?;
        if !scenario.forks.is_empty() {
            out.push_str("<h3>Fork boundaries</h3><ul>");
            for fork in &scenario.forks {
                write!(
                    out,
                    "<li>{} on {}: configured activation timestamp {}; {}.</li>",
                    Self::escape_html(&fork.name),
                    Self::escape_html(&fork.chain),
                    fork.activation_timestamp,
                    match (fork.observed_block, fork.observed_elapsed_ms) {
                        (Some(block), Some(ms)) => format!(
                            "boundary crossing observed at block {block}, {ms} ms (not proof of fork behavior)"
                        ),
                        _ => "boundary crossing not observed".into(),
                    }
                )?;
            }
            out.push_str("</ul>");
        }
        if !scenario.diagnostics.is_empty() {
            out.push_str("<h3>Collection limitations</h3><ul>");
            for diagnostic in &scenario.diagnostics {
                write!(out, "<li>{}</li>", Self::escape_html(diagnostic))?;
            }
            out.push_str("</ul>");
        }
        write!(
            out,
            "<h3>Inert reproduction command</h3><div class=\"kv\">{}</div></details></details>",
            Self::escape_html(&scenario.reproduction)
        )?;
        Ok(())
    }

    /// Renders a fixed head-progress SVG with explicit sample gaps.
    pub fn head_chart(scenario: &ScenarioResult) -> Result<String> {
        let numbered: Vec<_> =
            scenario.samples.iter().filter_map(|s| s.number.map(|n| (s.elapsed_ms, n))).collect();
        if numbered.is_empty() {
            return Ok("<div class=\"card\"><h3>Head progress</h3><p>No successful head observations; all samples are explicit gaps.</p></div>".into());
        }
        let min_x = 0;
        let max_x =
            scenario.samples.iter().map(|s| s.elapsed_ms).max().unwrap_or(min_x + 1).max(min_x + 1);
        let min_y = numbered.iter().map(|(_, n)| *n).min().unwrap_or(0);
        let max_y =
            numbered.iter().map(|(_, n)| *n).max().unwrap_or(min_y).max(min_y.saturating_add(1));
        let mut endpoints: Vec<_> = scenario.samples.iter().map(|s| &s.endpoint).collect();
        endpoints.sort();
        endpoints.dedup();
        let colors = ["#69d391", "#8ec5ff", "#ffbd66", "#c4b5fd"];
        let mut svg = String::from(
            "<div class=\"card\"><h3>Head progress</h3><p class=\"muted\">Height over elapsed time; lines stop at failed samples. Vertical separation at the same time is lag.</p><svg class=\"chart\" viewBox=\"0 0 1000 340\" role=\"img\" aria-label=\"Head heights by endpoint; gaps are not interpolated\"><path d=\"M60 20V300H980\" stroke=\"#66718c\" fill=\"none\"/>",
        );
        write!(
            svg,
            "<text x=\"60\" y=\"325\">{min_x} ms</text><text x=\"900\" y=\"325\">{max_x} ms</text><text x=\"5\" y=\"30\">block {max_y}</text><text x=\"5\" y=\"300\">block {min_y}</text>"
        )?;
        for (index, endpoint) in endpoints.iter().enumerate() {
            let samples: Vec<_> =
                scenario.samples.iter().filter(|s| &s.endpoint == *endpoint).collect();
            let mut segment = String::new();
            for sample in samples {
                if let Some(number) = sample.number {
                    let x = Self::scale(sample.elapsed_ms, min_x, max_x, 60, 980);
                    let y = 300 - Self::scale(number, min_y, max_y, 0, 280);
                    if segment.is_empty() {
                        write!(segment, "M{x} {y}")?;
                    } else {
                        write!(segment, " L{x} {y}")?;
                    }
                } else {
                    if !segment.is_empty() {
                        write!(
                            svg,
                            "<path class=\"line\" stroke=\"{}\" d=\"{}\"/>",
                            colors[index % colors.len()],
                            segment
                        )?;
                        segment.clear();
                    }
                    let x = Self::scale(sample.elapsed_ms, min_x, max_x, 60, 980);
                    write!(
                        svg,
                        "<path class=\"gap\" d=\"M{x} 20V300\"/><text class=\"gap-label\" x=\"{}\" y=\"45\">gap</text>",
                        x + 3
                    )?;
                }
            }
            if !segment.is_empty() {
                write!(
                    svg,
                    "<path class=\"line\" stroke=\"{}\" d=\"{}\"/>",
                    colors[index % colors.len()],
                    segment
                )?;
            }
            write!(
                svg,
                "<text x=\"{}\" y=\"{}\">{}</text>",
                650 + (index % 2) * 170,
                25 + (index / 2) * 18,
                Self::escape_html(endpoint)
            )?;
        }
        for fork in &scenario.forks {
            if let Some(ms) = fork.observed_elapsed_ms {
                let x = Self::scale(ms, min_x, max_x, 60, 980);
                write!(
                    svg,
                    "<path d=\"M{x} 20V300\" stroke=\"#ff8181\" stroke-dasharray=\"7 5\"/><text x=\"{}\" y=\"290\">{} boundary observed</text>",
                    x + 4,
                    Self::escape_html(&fork.name)
                )?;
            }
        }
        svg.push_str("</svg></div>");
        Ok(svg)
    }

    /// Renders a lifecycle table cell for matching stage records.
    pub fn stage_cell(scenario: &ScenarioResult, needle: &str) -> String {
        let stages: Vec<_> = scenario
            .stages
            .iter()
            .filter(|stage| stage.id.to_ascii_lowercase().contains(needle))
            .collect();
        if stages.is_empty() {
            return "<td class=\"muted\">— not recorded</td>".into();
        }
        let worst = stages
            .iter()
            .map(|stage| stage.status)
            .find(|s| *s != Status::Passed)
            .unwrap_or(Status::Passed);
        let duration: u64 = stages.iter().map(|stage| stage.duration_ms).sum();
        let messages = stages
            .iter()
            .filter(|stage| !stage.message.is_empty())
            .map(|stage| Self::escape_html(&stage.message))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "<td class=\"{}\">{} · {} ms{}</td>",
            worst.label(),
            worst.label(),
            duration,
            if messages.is_empty() {
                String::new()
            } else {
                format!("<br><span class=\"stage-message\">{messages}</span>")
            }
        )
    }

    /// Renders an accessible status table cell.
    pub fn status_html(status: Status) -> String {
        format!("<td class=\"{}\"><strong>{}</strong></td>", status.label(), status.label())
    }
    /// Serializes a JSON value for human-readable display.
    pub fn pretty_json(value: &Value) -> String {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into())
    }
    /// Serializes a JSON value on one line.
    pub fn compact_json(value: &Value) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| "null".into())
    }
    /// Formats an optional observation as a value or explicit gap.
    pub fn option_u64(value: Option<u64>) -> String {
        value.map_or_else(|| "gap".into(), |v| v.to_string())
    }
    /// Scales an integer into a bounded chart coordinate range.
    pub fn scale(value: u64, min: u64, max: u64, low: u64, high: u64) -> u64 {
        low + value.saturating_sub(min).saturating_mul(high - low) / max.saturating_sub(min).max(1)
    }
    /// Converts a validated identifier to a stable HTML anchor fragment.
    pub fn anchor(value: &str) -> String {
        value
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() { (b as char).to_ascii_lowercase() } else { '-' })
            .collect()
    }
    /// Escapes untrusted text for HTML text and quoted-attribute contexts.
    pub fn escape_html(value: &str) -> String {
        value
            .chars()
            .map(|c| match c {
                '&' => "&amp;".into(),
                '<' => "&lt;".into(),
                '>' => "&gt;".into(),
                '"' => "&quot;".into(),
                '\'' => "&#39;".into(),
                c if c.is_control() && !matches!(c, '\n' | '\t') => "�".into(),
                c => c.to_string(),
            })
            .collect()
    }
    /// Escapes untrusted text used in Markdown content.
    pub fn markdown_text(value: &str) -> String {
        value
            .chars()
            .map(|c| match c {
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '#' => format!("\\{c}"),
                c if c.is_control() && !matches!(c, '\n' | '\t') => "�".into(),
                c => c.to_string(),
            })
            .collect()
    }
    /// Validates a bounded portable record identifier.
    pub fn valid_id(kind: &str, value: &str) -> Result<()> {
        if value.is_empty()
            || value.len() > 200
            || !value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            bail!("invalid {kind} id {value:?}");
        }
        Ok(())
    }
    /// Rejects a string exceeding its byte budget.
    pub fn bounded(name: &str, value: &str, maximum: usize) -> Result<()> {
        if value.len() > maximum {
            bail!("{name} exceeds {maximum} bytes");
        }
        Ok(())
    }
    /// Validates a contained, URL-safe relative evidence path.
    pub fn validate_evidence_path(value: &str) -> Result<()> {
        Self::bounded("evidence path", value, 500)?;
        let path = Path::new(value);
        if value.is_empty()
            || value.contains('\\')
            || value.contains(['?', '#', '%'])
            || path.is_absolute()
            || !value.starts_with("evidence/")
            || path.components().any(|c| !matches!(c, Component::Normal(_)))
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
        {
            bail!("unsafe evidence path {value:?}");
        }
        Ok(())
    }
    /// Rejects existing evidence links which resolve outside the report bundle.
    pub fn validate_existing_evidence(run: &RunResult, directory: &Path) -> Result<()> {
        let root = directory.canonicalize().wrap_err("canonicalize report directory")?;
        for path in run
            .scenarios
            .iter()
            .flat_map(|scenario| &scenario.checks)
            .flat_map(|check| &check.evidence)
        {
            let candidate = directory.join(path);
            if candidate.symlink_metadata().is_ok() {
                let resolved = candidate
                    .canonicalize()
                    .wrap_err_with(|| format!("resolve evidence path {path}"))?;
                if !resolved.starts_with(&root) {
                    bail!("evidence path escapes report directory: {path}");
                }
            }
        }
        Ok(())
    }
    /// Rejects common credential forms; collection must still use allowlists.
    pub fn reject_secret(name: &str, value: &str) -> Result<()> {
        let lower = value.to_ascii_lowercase();
        let bearer_token = lower.find("bearer ").is_some_and(|start| {
            value[start + 7..].split_whitespace().next().is_some_and(|token| {
                token.len() >= 16
                    && token.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            })
        });
        if lower.contains("begin private key")
            || bearer_token
            || lower.contains("://")
                && value.split("://").nth(1).is_some_and(|tail| {
                    tail.split('/').next().is_some_and(|authority| authority.contains('@'))
                })
        {
            bail!("possible credential in {name}");
        }
        Ok(())
    }
    /// Validates bounds and credential patterns recursively in JSON display data.
    pub fn validate_value(value: &Value, depth: usize) -> Result<()> {
        if depth > 20 {
            bail!("JSON value nesting exceeds 20");
        }
        match value {
            Value::String(v) => {
                Self::bounded("JSON string", v, 65_536)?;
                Self::reject_secret("JSON string", v)?;
            }
            Value::Array(values) => {
                if values.len() > 10_000 {
                    bail!("JSON array too large");
                }
                for v in values {
                    Self::validate_value(v, depth + 1)?;
                }
            }
            Value::Object(values) => {
                if values.len() > 1_000 {
                    bail!("JSON object too large");
                }
                for (k, v) in values {
                    Self::bounded("JSON key", k, 200)?;
                    Self::validate_value(v, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckResult, ForkBoundary, HeadSample, ScenarioResult, StageResult};
    use serde_json::json;

    fn run(status: Status) -> RunResult {
        RunResult {
            schema_version: 1,
            run_id: "synthetic-test".into(),
            tested_sha: "abc123".into(),
            started_at_unix_ms: 1,
            scenarios: vec![ScenarioResult {
                id: "smoke".into(),
                status: Status::Passed,
                duration_ms: 10,
                config: json!({"network":"synthetic"}),
                stages: vec![StageResult {
                    id: "setup".into(),
                    status: Status::Passed,
                    duration_ms: 2,
                    message: String::new(),
                }],
                checks: vec![CheckResult {
                    id: "heads".into(),
                    kind: "heads_converge".into(),
                    status,
                    duration_ms: 5,
                    expected: json!({"max_lag":5}),
                    observed: json!({"lag":14}),
                    message: "result".into(),
                    next_step: "inspect logs".into(),
                    samples: 2,
                    rpc_errors: 0,
                    evidence: vec!["evidence/heads.json".into()],
                }],
                samples: vec![
                    HeadSample {
                        endpoint: "builder".into(),
                        elapsed_ms: 0,
                        number: Some(100),
                        timestamp: Some(1),
                        hash: Some("0xaa".into()),
                    },
                    HeadSample {
                        endpoint: "builder".into(),
                        elapsed_ms: 10,
                        number: None,
                        timestamp: None,
                        hash: None,
                    },
                    HeadSample {
                        endpoint: "builder".into(),
                        elapsed_ms: 20,
                        number: Some(102),
                        timestamp: Some(2),
                        hash: Some("0xbb".into()),
                    },
                ],
                forks: vec![ForkBoundary {
                    name: "denim".into(),
                    chain: "l2".into(),
                    activation_timestamp: 2,
                    observed_block: Some(102),
                    observed_elapsed_ms: Some(20),
                }],
                diagnostics: vec![],
                reproduction: "base-acceptance run synthetic.toml".into(),
            }],
        }
    }

    #[test]
    fn derives_counts_and_ignores_supplied_status() {
        let mut value = run(Status::Failed);
        value.scenarios[0].status = Status::Passed;
        assert_eq!(
            ReportCounts::from_run(&value),
            ReportCounts { failed: 1, ..Default::default() }
        );
        assert!(Report::html(&value).unwrap().contains("FAILED"));
    }
    #[test]
    fn escapes_hostile_content_in_both_formats() {
        let mut value = run(Status::Failed);
        value.scenarios[0].checks[0].message = "<script>*boom* | [x]".into();
        let html = Report::html(&value).unwrap();
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        let md = Report::markdown(&value).unwrap();
        assert!(md.contains("\\*boom\\* \\| \\[x\\]"));
    }
    #[test]
    fn rejects_schema_duplicates_paths_secrets_and_large_input() {
        let mut value = run(Status::Passed);
        value.schema_version = 2;
        assert!(Report::html(&value).is_err());
        value.schema_version = 1;
        value.scenarios.push(value.scenarios[0].clone());
        assert!(Report::html(&value).is_err());
        value.scenarios.pop();
        value.scenarios[0].checks[0].evidence[0] = "evidence/../secret".into();
        assert!(Report::html(&value).is_err());
        value.scenarios[0].checks[0].evidence[0] = "evidence/a".into();
        value.scenarios[0].reproduction = "Bearer abcdefghijklmnopqrstuvwxyz012345".into();
        assert!(Report::html(&value).is_err());
        value.scenarios[0].reproduction = "diagnostic mentions private_key keyword".into();
        assert!(Report::html(&value).is_ok());
        value.scenarios[0].reproduction = "x".repeat(4097);
        assert!(Report::html(&value).is_err());
    }
    #[test]
    fn chart_breaks_lines_at_explicit_gap_and_labels_boundary() {
        let chart = Report::head_chart(&run(Status::Passed).scenarios[0]).unwrap();
        assert_eq!(chart.matches("class=\"line\"").count(), 2);
        assert!(chart.contains("gap") && chart.contains("boundary observed"));
    }
    #[test]
    fn empty_results_and_startup_blocked_are_not_green() {
        let mut value = run(Status::Blocked);
        value.scenarios[0].checks.clear();
        assert_eq!(value.scenarios[0].outcome(), Status::Error);
        value.scenarios[0].checks.push(CheckResult {
            status: Status::Blocked,
            ..run(Status::Blocked).scenarios.remove(0).checks.remove(0)
        });
        value.scenarios[0].stages[0].status = Status::Error;
        assert_eq!(ReportCounts::verdict(&value), Status::Error);
        assert!(Report::html(&value).unwrap().contains("INCOMPLETE / INFRASTRUCTURE ERROR"));
        value.scenarios.clear();
        assert!(Report::html(&value).is_err());
    }
    #[test]
    fn rendering_is_deterministic_and_markdown_bounded() {
        let value = run(Status::Failed);
        assert_eq!(Report::html(&value).unwrap(), Report::html(&value).unwrap());
        let mut large = value;
        large.scenarios[0].checks = (0..1000)
            .map(|i| {
                let mut c = run(Status::Failed).scenarios.remove(0).checks.remove(0);
                c.id = format!("check-{i}");
                c.message = "x".repeat(200);
                c
            })
            .collect();
        let md = Report::markdown(&large).unwrap();
        assert!(md.len() < 50 * 1024);
        assert!(md.contains("omitted"));
    }
    #[test]
    fn write_preserves_evidence_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("evidence")).unwrap();
        fs::write(dir.path().join("evidence/keep"), "yes").unwrap();
        Report::write(&run(Status::Passed), dir.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("evidence/keep")).unwrap(), "yes");
        assert!(dir.path().join("report.html").exists());
    }

    #[test]
    fn reports_exact_mixed_counts_and_remediation() {
        let mut value = run(Status::Failed);
        let mut error = value.scenarios[0].checks[0].clone();
        error.id = "rpc-error".into();
        error.status = Status::Error;
        error.evidence.clear();
        value.scenarios[0].checks.push(error);
        value.scenarios[0].stages[0].status = Status::Error;
        value.scenarios[0].stages[0].message = "setup unavailable".into();

        assert_eq!(
            ReportCounts::from_run(&value),
            ReportCounts { failed: 1, error: 1, ..ReportCounts::default() }
        );
        let markdown = Report::markdown(&value).unwrap();
        assert!(markdown.contains("Next step: inspect logs"));
        assert!(markdown.contains("Reproduce: `base-acceptance run synthetic.toml`"));
        assert!(markdown.contains("⚠ none recorded"));
        let html = Report::html(&value).unwrap();
        assert!(html.contains("setup unavailable"));
        assert!(html.contains("<td class=\"error\"><strong>error</strong></td>"));
    }

    #[cfg(unix)]
    #[test]
    fn write_rejects_symlink_destinations() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), dir.path().join("result.json")).unwrap();
        assert!(Report::write(&run(Status::Passed), dir.path()).is_err());
        assert!(fs::read_to_string(outside.path()).unwrap().is_empty());
    }
}
