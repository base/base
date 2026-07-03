use std::collections::HashSet;

use async_trait::async_trait;
use tree_sitter::{Query, QueryCursor};

use super::{Check, CheckResult, FileContext, Issue, IssueCategory, Severity};
use crate::languages::Language;

#[derive(Debug, Clone)]
struct Import {
    name: String,
    line: u32,
    column: u32,
}

pub struct DependencyCheck {
    known_packages: HashSet<String>,
}

impl DependencyCheck {
    pub fn new() -> Self {
        let mut known = HashSet::new();
        known.insert("serde".to_string());
        known.insert("tokio".to_string());
        known.insert("anyhow".to_string());
        known.insert("requests".to_string());
        known.insert("express".to_string());
        known.insert("react".to_string());

        Self { known_packages: known }
    }
}

impl Default for DependencyCheck {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Check for DependencyCheck {
    fn name(&self) -> &str {
        "dependency-provenance"
    }
    fn category(&self) -> IssueCategory {
        IssueCategory::Dependency
    }

    async fn execute(&self, ctx: &FileContext) -> anyhow::Result<CheckResult> {
        let imports = self.extract_imports(ctx)?;
        let mut issues = Vec::new();

        for imp in &imports {
            if self.known_packages.contains(&imp.name) {
                continue;
            }

            if self.is_likely_hallucinated(&imp.name, ctx.language) {
                issues.push(Issue {
                    line: imp.line,
                    column: imp.column,
                    severity: Severity::Warning,
                    message: format!(
                        "Dependência '{}' parece ser alucinação (offline, sem confirmação do registry)",
                        imp.name
                    ),
                    category: IssueCategory::Dependency,
                });
            }
        }

        let score = if imports.is_empty() {
            1.0
        } else {
            1.0 - (issues.len() as f64 / imports.len() as f64)
        };

        Ok(CheckResult { passed: issues.is_empty(), issues, suggestions: vec![], score })
    }
}

impl DependencyCheck {
    fn is_likely_hallucinated(&self, name: &str, lang: Language) -> bool {
        let generic_suffixes = ["-utils", "-helpers", "-core", "-tools", "-lib"];
        let is_generic = generic_suffixes.iter().any(|s| name.ends_with(s));

        let too_perfect = match lang {
            Language::Python => name.starts_with("python-") && name.len() > 8,
            Language::JavaScript => {
                (name.ends_with("-js") || name.starts_with("js-")) && name.len() > 5
            }
            _ => false,
        };

        is_generic && too_perfect
    }

    fn extract_imports(&self, ctx: &FileContext) -> anyhow::Result<Vec<Import>> {
        let query_src = match ctx.language {
            Language::Rust => {
                r#"
                (use_declaration argument: (scoped_use_path path:(_) @path))
            "#
            }
            Language::Python => {
                r#"
                (import_statement name: (dotted_name) @path)
                (import_from_statement module_name: (dotted_name) @path)
            "#
            }
            Language::JavaScript | Language::TypeScript => {
                r#"
                (import_statement source: (string) @path)
                (call_expression function: (identifier) @fn (#eq? @fn "require")
                    arguments: (arguments (string) @path))
            "#
            }
            Language::Go => {
                r#"
                (import_declaration
                    (import_spec path: (interpreted_string_literal) @path))
            "#
            }
            _ => return Ok(vec![]),
        };

        let language = ctx.language.tree_sitter_language();
        let Ok(query) = Query::new(&language, query_src) else {
            return Ok(vec![]);
        };

        let mut cursor = QueryCursor::new();
        let imports: Vec<Import> = cursor
            .matches(&query, ctx.tree.root_node(), ctx.code.as_bytes())
            .filter_map(|m| {
                let node = m.captures[0].node;
                let raw = &ctx.code[node.byte_range()];
                let name = self.extract_package_name(raw, ctx.language)?;
                Some(Import {
                    name,
                    line: node.start_position().row as u32 + 1,
                    column: node.start_position().column as u32,
                })
            })
            .collect();

        Ok(imports)
    }

    fn extract_package_name(&self, raw: &str, lang: Language) -> Option<String> {
        match lang {
            Language::Rust => raw.split("::").next().map(|s| s.to_string()),
            Language::Python => raw.split('.').next().map(|s| s.to_string()),
            Language::JavaScript | Language::TypeScript => {
                let cleaned = raw.trim_matches('"').trim_matches('\'');
                Some(cleaned.split('/').next()?.to_string())
            }
            Language::Go => {
                let cleaned = raw.trim_matches('"');
                cleaned.split('/').next_back().map(|s| s.to_string())
            }
            _ => None,
        }
    }
}
