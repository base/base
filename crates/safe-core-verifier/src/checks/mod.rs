use async_trait::async_trait;

use crate::FileContext;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Issue {
    pub line: u32,
    pub column: u32,
    pub severity: Severity,
    pub message: String,
    pub category: IssueCategory,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum IssueCategory {
    ConventionX,
    Dependency,
    Safety,
    Context,
    Other,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub passed: bool,
    pub issues: Vec<Issue>,
    pub suggestions: Vec<String>,
    pub score: f64,
}

impl Default for CheckResult {
    fn default() -> Self {
        Self { passed: true, issues: Vec::new(), suggestions: Vec::new(), score: 1.0 }
    }
}

#[async_trait]
pub trait Check: Send + Sync {
    fn name(&self) -> &str;
    fn category(&self) -> IssueCategory;
    async fn execute(&self, ctx: &FileContext) -> anyhow::Result<CheckResult>;
}

pub struct AllChecks(pub Vec<Box<dyn Check>>);

#[async_trait]
impl Check for AllChecks {
    fn name(&self) -> &str {
        "all-checks"
    }
    fn category(&self) -> IssueCategory {
        IssueCategory::Other
    }

    async fn execute(&self, ctx: &FileContext) -> anyhow::Result<CheckResult> {
        let mut all_issues = Vec::new();
        let mut all_suggestions = Vec::new();
        let mut total_score = 0.0;
        let mut count = 0;

        for check in &self.0 {
            let result = check.execute(ctx).await?;
            all_issues.extend(result.issues);
            all_suggestions.extend(result.suggestions);
            total_score += result.score;
            count += 1;
        }

        Ok(CheckResult {
            passed: all_issues.is_empty(),
            issues: all_issues,
            suggestions: all_suggestions,
            score: if count > 0 { total_score / count as f64 } else { 1.0 },
        })
    }
}

pub mod convention_x;
pub mod dependency;
pub mod safety;
