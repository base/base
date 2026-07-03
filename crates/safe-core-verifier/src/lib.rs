pub mod cgf;
pub mod checks;
pub mod constraint;
pub mod context;
pub mod languages;
pub mod lean4;
pub mod report;

pub use checks::Check;
pub use constraint::{Constraint, ConstraintResult};
pub use context::FileContext;
pub use lean4::Lean4Verifier;
pub use report::{FileReport, GlobalReport};

/// Verificador de restrições via Lean4.
pub trait Verifier: Send + Sync {
    fn verify(&self, constraint: &Constraint, context: &serde_json::Value) -> ConstraintResult;
}

pub struct PolyglotVerifier {
    checks: Vec<Box<dyn checks::Check>>,
}

impl PolyglotVerifier {
    pub fn new(checks: Vec<Box<dyn checks::Check>>) -> Self {
        Self { checks }
    }

    pub async fn verify_project(&self, root: &str) -> anyhow::Result<GlobalReport> {
        let mut reports = Vec::new();

        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                if let Ok(lang) = languages::Language::detect(entry.path()) {
                    let content = std::fs::read_to_string(entry.path())?;
                    let mut parser = tree_sitter::Parser::new();
                    parser.set_language(&lang.tree_sitter_language()).unwrap();
                    let tree = parser.parse(&content, None).unwrap();
                    let ctx = context::FileContext {
                        path: entry.path().to_path_buf(),
                        language: lang,
                        code: content,
                        tree,
                        content_hash: 0,
                    };
                    let mut issues = Vec::new();
                    for check in &self.checks {
                        let res = check.execute(&ctx).await?;
                        issues.extend(res.issues);
                    }
                    reports.push(FileReport {
                        path: entry.path().display().to_string(),
                        language: format!("{:?}", ctx.language),
                        alpha_hat: 0.0,
                        passed: issues.is_empty(),
                        issues,
                        suggestions: vec![],
                    });
                }
            }
        }

        Ok(GlobalReport::from_file_reports(reports))
    }
}
