use async_trait::async_trait;
use tree_sitter::{Query, QueryCursor};

use super::{Check, CheckResult, Issue, IssueCategory, Severity};
use crate::{FileContext, languages::Language};

pub struct SafetyCheck;

#[async_trait]
impl Check for SafetyCheck {
    fn name(&self) -> &str {
        "safety-patterns"
    }
    fn category(&self) -> IssueCategory {
        IssueCategory::Safety
    }

    async fn execute(&self, ctx: &FileContext) -> anyhow::Result<CheckResult> {
        let patterns: &[(&str, &str, Severity)] = match ctx.language {
            Language::Rust => &[
                (
                    r#"(method_invocation object:(_) method:(identifier) @m (#eq? @m "unwrap"))"#,
                    ".unwrap() sem tratamento de erro",
                    Severity::Error,
                ),
                (
                    r#"(method_invocation object:(_) method:(identifier) @m (#eq? @m "expect"))"#,
                    ".expect() — use Result<> com ?",
                    Severity::Warning,
                ),
                (r#"(unsafe_block)"#, "Bloco unsafe detectado", Severity::Error),
            ],
            Language::Python => &[
                (
                    r#"(call function:(_) @f (#match? @f "^eval$|^exec$|^compile$"))"#,
                    "Uso de eval/exec/compile — injeção de código",
                    Severity::Error,
                ),
                (
                    r#"(call function:(_) @f (#match? @f "^pickle\\.loads$|^yaml\\.load$"))"#,
                    "Desserialização insegura — use safe_load",
                    Severity::Error,
                ),
                (
                    r#"(expression_statement (call function:(_) @f (#match? @f "^subprocess\\.")))"#,
                    "subprocess sem validação — risco de injeção de comando",
                    Severity::Warning,
                ),
            ],
            Language::JavaScript | Language::TypeScript => &[
                (
                    r#"(call_expression function:(_) @f (#match? @f "^eval$|^Function$"))"#,
                    "eval() ou new Function() — injeção de código",
                    Severity::Error,
                ),
                (
                    r#"(call_expression function:(_) @f (#match? @f "^innerHTML$"))"#,
                    "innerHTML — XSS potencial",
                    Severity::Error,
                ),
                (
                    r#"(call_expression function:(member_expression object:(identifier) @o property:(identifier) @p (#eq? @o "JSON") (#eq? @p "parse")))"#,
                    "JSON.parse sem try/catch — pode lançar",
                    Severity::Info,
                ),
            ],
            Language::Go => &[(
                r#"(call_expression function:(_) @f (#match? @f "^exec\\.Command$"))"#,
                "exec.Command — injeção de comando potencial",
                Severity::Warning,
            )],
            _ => &[],
        };

        let language = ctx.language.tree_sitter_language();
        let mut all_issues = Vec::new();

        for (query_src, message, severity) in patterns {
            let Ok(query) = Query::new(&language, query_src) else { continue };
            let mut cursor = QueryCursor::new();
            for m in cursor.matches(&query, ctx.tree.root_node(), ctx.code.as_bytes()) {
                let node = m.captures[0].node;
                all_issues.push(Issue {
                    line: node.start_position().row as u32 + 1,
                    column: node.start_position().column as u32,
                    severity: severity.clone(),
                    message: (*message).to_string(),
                    category: IssueCategory::Safety,
                });
            }
        }

        let score = 1.0
            - all_issues
                .iter()
                .map(|i| match i.severity {
                    Severity::Error => 0.3,
                    Severity::Warning => 0.1,
                    _ => 0.05,
                })
                .sum::<f64>()
                .min(1.0);

        Ok(CheckResult {
            passed: all_issues.iter().all(|i| matches!(i.severity, Severity::Info)),
            issues: all_issues,
            suggestions: vec![],
            score,
        })
    }
}
