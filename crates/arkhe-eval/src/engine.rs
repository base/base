//! Engine principal de avaliação

use crate::dimensions::{Dimension, DimensionScore, CriterionResult, CheckType, Criterion};
use crate::report::{EvalReport, MaturityLevel};
use crate::error::{EvalError, Result};
use std::path::PathBuf;
use std::process::Command;
use tracing::{info, warn, debug};

#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub workspace_path: PathBuf,
    pub verbose: bool,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            workspace_path: PathBuf::from("."),
            verbose: false,
        }
    }
}

pub struct EvalEngine {
    config: EvalConfig,
    dimensions: Vec<Dimension>,
}

impl EvalEngine {
    pub fn new(config: EvalConfig) -> Self {
        let dimensions = Self::build_dimensions();
        Self { config, dimensions }
    }

    pub async fn evaluate(&self) -> Result<EvalReport> {
        info!("Starting ARKHE evaluation");
        info!("Workspace: {}", self.config.workspace_path.display());

        if !self.config.workspace_path.exists() {
            return Err(EvalError::WorkspaceNotFound(
                self.config.workspace_path.display().to_string()
            ));
        }

        let mut dimension_scores = Vec::new();
        let mut total_score = 0.0;

        for dimension in &self.dimensions {
            info!("Evaluating dimension: {}", dimension.name);
            let score = self.evaluate_dimension(dimension).await?;
            total_score += score.weighted_score;
            dimension_scores.push(score);
        }

        let level = MaturityLevel::from_score(total_score);

        info!("Evaluation complete: {:.1}/100", total_score);
        info!("Maturity level: {:?}", level);

        Ok(EvalReport {
            generated_at: chrono::Utc::now(),
            total_score,
            level,
            dimensions: dimension_scores,
        })
    }

    async fn evaluate_dimension(&self, dimension: &Dimension) -> Result<DimensionScore> {
        let mut criteria_results = Vec::new();
        let mut raw_score = 0.0;

        for criterion in &dimension.criteria {
            let result = self.evaluate_criterion(criterion).await?;
            raw_score += result.points_earned;
            criteria_results.push(result);
        }

        let weighted_score = raw_score * dimension.weight;

        Ok(DimensionScore {
            dimension_id: dimension.id.clone(),
            dimension_name: dimension.name.clone(),
            raw_score,
            weighted_score,
            criteria_results,
        })
    }

    async fn evaluate_criterion(&self, criterion: &crate::dimensions::Criterion) -> Result<CriterionResult> {
        debug!("Evaluating criterion: {}", criterion.description);

        let (passed, points, evidence) = match &criterion.check_type {
            CheckType::FileExists { path } => {
                let full_path = self.config.workspace_path.join(path);
                let exists = full_path.exists();
                let points = if exists { criterion.max_points } else { 0.0 };
                let evidence = if exists {
                    format!("File exists: {}", full_path.display())
                } else {
                    format!("File not found: {}", full_path.display())
                };
                (exists, points, evidence)
            }

            CheckType::DirExists { path } => {
                let full_path = self.config.workspace_path.join(path);
                let exists = full_path.is_dir();
                let points = if exists { criterion.max_points } else { 0.0 };
                let evidence = if exists {
                    format!("Directory exists: {}", full_path.display())
                } else {
                    format!("Directory not found: {}", full_path.display())
                };
                (exists, points, evidence)
            }

            CheckType::FileContains { path, text } => {
                let full_path = self.config.workspace_path.join(path);
                let contains = if full_path.exists() {
                    std::fs::read_to_string(&full_path)
                        .map(|content| content.contains(text))
                        .unwrap_or(false)
                } else {
                    false
                };
                let points = if contains { criterion.max_points } else { 0.0 };
                let evidence = if contains {
                    format!("File contains '{}' in {}", text, full_path.display())
                } else {
                    format!("Text '{}' not found in {}", text, full_path.display())
                };
                (contains, points, evidence)
            }

            CheckType::CommandSuccess { command } => {
                let parts: Vec<&str> = command.split_whitespace().collect();
                if parts.is_empty() {
                    (false, 0.0, "Empty command".to_string())
                } else {
                    let result = Command::new(parts[0])
                        .args(&parts[1..])
                        .current_dir(&self.config.workspace_path)
                        .output();

                    match result {
                        Ok(output) => {
                            let success = output.status.success();
                            let points = if success { criterion.max_points } else { 0.0 };
                            let evidence = if success {
                                "Command succeeded".to_string()
                            } else {
                                format!("Command failed: {}", String::from_utf8_lossy(&output.stderr))
                            };
                            (success, points, evidence)
                        }
                        Err(e) => {
                            (false, 0.0, format!("Command error: {}", e))
                        }
                    }
                }
            }

            CheckType::Custom { checker } => {
                warn!("Custom checker not implemented: {}", checker);
                (false, 0.0, format!("Custom checker '{}' not implemented", checker))
            }
        };

        Ok(CriterionResult {
            criterion_id: criterion.id.clone(),
            description: criterion.description.clone(),
            points_earned: points,
            max_points: criterion.max_points,
            passed,
            evidence,
        })
    }

    pub fn build_dimensions() -> Vec<Dimension> {
        vec![
            // 1. Arquitetura Conceitual (15%)
            Dimension::new("D1", "Arquitetura Conceitual", 0.15)
                .with_criterion(Criterion::new(
                    "D1-C1", "Cargo.toml existe", 3.0,
                    CheckType::FileExists { path: "Cargo.toml".into() }
                ))
                .with_criterion(Criterion::new(
                    "D1-C2", "Diretório crates existe", 3.0,
                    CheckType::DirExists { path: "crates".into() }
                ))
                .with_criterion(Criterion::new(
                    "D1-C3", "Diretório src existe", 3.0,
                    CheckType::DirExists { path: "src".into() }
                )),

            // 2. Especificação Formal (20%)
            Dimension::new("D2", "Especificação Formal", 0.20)
                .with_criterion(Criterion::new(
                    "D2-C1", "Documentação README existe", 5.0,
                    CheckType::FileExists { path: "README.md".into() }
                ))
                .with_criterion(Criterion::new(
                    "D2-C2", "Diretório docs existe", 5.0,
                    CheckType::DirExists { path: "docs".into() }
                ))
                .with_criterion(Criterion::new(
                    "D2-C3", "Cargo.toml contém description", 5.0,
                    CheckType::FileContains {
                        path: "Cargo.toml".into(),
                        text: "description".into()
                    }
                )),

            // 3. Compilabilidade (20%)
            Dimension::new("D3", "Compilabilidade", 0.20)
                .with_criterion(Criterion::new(
                    "D3-C1", "cargo check executa com sucesso", 10.0,
                    CheckType::CommandSuccess { command: "cargo check".into() }
                ))
                .with_criterion(Criterion::new(
                    "D3-C2", "Cargo.lock existe", 5.0,
                    CheckType::FileExists { path: "Cargo.lock".into() }
                ))
                .with_criterion(Criterion::new(
                    "D3-C3", "Sem warnings de compilação", 5.0,
                    CheckType::CommandSuccess { command: "cargo clippy -- -D warnings".into() }
                )),

            // 4. Testabilidade (15%)
            Dimension::new("D4", "Testabilidade", 0.15)
                .with_criterion(Criterion::new(
                    "D4-C1", "cargo test executa com sucesso", 8.0,
                    CheckType::CommandSuccess { command: "cargo test".into() }
                ))
                .with_criterion(Criterion::new(
                    "D4-C2", "Diretório tests existe", 4.0,
                    CheckType::DirExists { path: "tests".into() }
                ))
                .with_criterion(Criterion::new(
                    "D4-C3", "Cargo.toml contém dev-dependencies", 3.0,
                    CheckType::FileContains {
                        path: "Cargo.toml".into(),
                        text: "dev-dependencies".into()
                    }
                )),

            // 5. Integração (15%)
            Dimension::new("D5", "Integração", 0.15)
                .with_criterion(Criterion::new(
                    "D5-C1", "Diretório examples existe", 5.0,
                    CheckType::DirExists { path: "examples".into() }
                ))
                .with_criterion(Criterion::new(
                    "D5-C2", "Cargo.toml contém dependencies", 5.0,
                    CheckType::FileContains {
                        path: "Cargo.toml".into(),
                        text: "dependencies".into()
                    }
                ))
                .with_criterion(Criterion::new(
                    "D5-C3", "Exemplos compilam", 5.0,
                    CheckType::CommandSuccess { command: "cargo build --examples".into() }
                )),

            // 6. Documentação (5%)
            Dimension::new("D6", "Documentação", 0.05)
                .with_criterion(Criterion::new(
                    "D6-C1", "README contém instruções de uso", 3.0,
                    CheckType::FileContains {
                        path: "README.md".into(),
                        text: "## Uso".into()
                    }
                ))
                .with_criterion(Criterion::new(
                    "D6-C2", "README contém exemplos", 2.0,
                    CheckType::FileContains {
                        path: "README.md".into(),
                        text: "```rust".into()
                    }
                )),

            // 7. Maturidade Operacional (10%)
            Dimension::new("D7", "Maturidade Operacional", 0.10)
                .with_criterion(Criterion::new(
                    "D7-C1", "Diretório .github/workflows existe", 3.0,
                    CheckType::DirExists { path: ".github/workflows".into() }
                ))
                .with_criterion(Criterion::new(
                    "D7-C2", "Cargo.toml contém license", 3.0,
                    CheckType::FileContains {
                        path: "Cargo.toml".into(),
                        text: "license".into()
                    }
                ))
                .with_criterion(Criterion::new(
                    "D7-C3", "Cargo.toml contém authors", 2.0,
                    CheckType::FileContains {
                        path: "Cargo.toml".into(),
                        text: "authors".into()
                    }
                )),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_config_default() {
        let config = EvalConfig::default();
        assert_eq!(config.workspace_path, PathBuf::from("."));
        assert!(!config.verbose);
    }

    #[test]
    fn test_build_dimensions() {
        let dimensions = EvalEngine::build_dimensions();
        assert_eq!(dimensions.len(), 7);

        let total_weight: f64 = dimensions.iter().map(|d| d.weight).sum();
        assert!((total_weight - 1.0).abs() < 0.001);
    }
}
