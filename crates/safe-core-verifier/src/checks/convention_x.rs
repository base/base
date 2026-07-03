use async_trait::async_trait;
use tree_sitter::{Query, QueryCursor};

use super::{Check, CheckResult, Issue, IssueCategory, Severity};
use crate::{FileContext, languages::Language};

pub struct ConventionXCheck;

#[async_trait]
impl Check for ConventionXCheck {
    fn name(&self) -> &str {
        "convention-x"
    }
    fn category(&self) -> IssueCategory {
        IssueCategory::ConventionX
    }

    async fn execute(&self, ctx: &FileContext) -> anyhow::Result<CheckResult> {
        let query_source = match ctx.language {
            Language::Rust => {
                r#"
                (
                    function_item
                    name: (identifier) @fn_name
                    parameters: (parameters
                        (parameter
                            type: (_)
                        ) @param
                    )
                    (#not-match? @fn_name "^x_")
                    (#match? @param "String|Vec<|HashMap<|&\\[")
                )
            "#
            }
            Language::Python => {
                r#"
                (
                    function_definition
                    name: (identifier) @fn_name
                    parameters: (parameters
                        (parameter
                            type: (_)
                        ) @param
                    )
                    (#not-match? @fn_name "^x_")
                    (#match? @param "str|list|dict|Any")
                )
            "#
            }
            Language::JavaScript | Language::TypeScript => {
                r#"
                (
                    function_declaration
                    name: (identifier) @fn_name
                    parameters: (formal_parameters) @params
                    (#not-match? @fn_name "^x_")
                )
            "#
            }
            Language::Go => {
                r#"
                (
                    function_declaration
                    name: (identifier) @fn_name
                    parameters: (parameter_list
                        (parameter_declaration
                            type: (_)
                        ) @param
                    )
                    (#match? @fn_name "^[A-Z]")
                    (#not-match? @fn_name "^X_")
                    (#match? @param "string|\\[\\]|map\\[")
                )
            "#
            }
            _ => return Ok(CheckResult::default()),
        };

        let language = ctx.language.tree_sitter_language();
        let query = match Query::new(&language, query_source) {
            Ok(q) => q,
            Err(_) => {
                return Ok(CheckResult::default());
            }
        };

        let mut cursor = QueryCursor::new();
        let matches: Vec<_> =
            cursor.matches(&query, ctx.tree.root_node(), ctx.code.as_bytes()).collect();

        let issues: Vec<Issue> = matches
            .iter()
            .map(|m| {
                let name_node = m.captures[0].node;
                Issue {
                    line: name_node.start_position().row as u32 + 1,
                    column: name_node.start_position().column as u32,
                    severity: Severity::Error,
                    message: format!(
                        "Função '{}' recebe dados externos sem prefixo de fronteira",
                        &ctx.code[name_node.byte_range()]
                    ),
                    category: IssueCategory::ConventionX,
                }
            })
            .collect();

        let passed = issues.is_empty();
        let score = if passed { 1.0 } else { 0.0_f64.max(1.0 - issues.len() as f64 * 0.2) };

        Ok(CheckResult {
            passed,
            issues: issues.clone(),
            suggestions: issues
                .iter()
                .map(|i| {
                    format!(
                        "Renomeie '{}' para marcar fronteira de confiança (prefixo x_)",
                        i.message.split('\'').nth(1).unwrap_or("?")
                    )
                })
                .collect(),
            score,
        })
    }
}
