//! Relatório de avaliação e níveis de maturidade

use crate::dimensions::DimensionScore;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaturityLevel {
    Inexistent,
    Prototype,
    Functional,
    Validated,
    Production,
    Sovereign,
}

impl MaturityLevel {
    pub fn from_score(score: f64) -> Self {
        if score < 10.0 {
            Self::Inexistent
        } else if score < 30.0 {
            Self::Prototype
        } else if score < 50.0 {
            Self::Functional
        } else if score < 70.0 {
            Self::Validated
        } else if score < 85.0 {
            Self::Production
        } else {
            Self::Sovereign
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Inexistent => "Inexistente — Apenas ideias, sem código",
            Self::Prototype => "Protótipo — Código existe, mas não compila",
            Self::Functional => "Funcional — Compila, mas sem testes",
            Self::Validated => "Validado — Compila + testes + documentação",
            Self::Production => "Produção — Deploy + monitoramento + CI/CD",
            Self::Sovereign => "Soberano — Autoavaliação contínua + evolução autônoma",
        }
    }

    pub fn emoji(&self) -> &'static str {
        match self {
            Self::Inexistent => "💀",
            Self::Prototype => "🔬",
            Self::Functional => "⚙️",
            Self::Validated => "🧪",
            Self::Production => "🚀",
            Self::Sovereign => "👑",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub generated_at: DateTime<Utc>,
    pub total_score: f64,
    pub level: MaturityLevel,
    pub dimensions: Vec<DimensionScore>,
}

impl EvalReport {
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        md.push_str(&format!("# {} ARKHE Evaluation Report\n\n", self.level.emoji()));
        md.push_str(&format!("**Generated:** {}\n", self.generated_at.format("%Y-%m-%d %H:%M:%S UTC")));
        md.push_str(&format!("**Total Score:** {:.1}/100\n", self.total_score));
        md.push_str(&format!("**Maturity Level:** {:?}\n\n", self.level));
        md.push_str(&format!("{}\n\n", self.level.description()));

        md.push_str("## Dimension Scores\n\n");
        md.push_str("| Dimension | Score | Weight | Weighted | % |\n");
        md.push_str("|-----------|-------|--------|----------|---|\n");

        for dim in &self.dimensions {
            let percentage = dim.calculate_percentage();
            md.push_str(&format!(
                "| {} | {:.1} | {:.0}% | {:.2} | {:.1}% |\n",
                dim.dimension_name,
                dim.raw_score,
                100.0,
                dim.weighted_score,
                percentage
            ));
        }

        md.push_str("\n## Detailed Results\n\n");

        for dim in &self.dimensions {
            md.push_str(&format!("### {}\n\n", dim.dimension_name));

            for criterion in &dim.criteria_results {
                let status = if criterion.passed { "✅" } else { "❌" };
                md.push_str(&format!(
                    "- {} **{}**: {} ({:.1}/{:.1})\n",
                    status,
                    criterion.description,
                    criterion.evidence,
                    criterion.points_earned,
                    criterion.max_points
                ));
            }
            md.push_str("\n");
        }

        md
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maturity_level_from_score() {
        assert_eq!(MaturityLevel::from_score(5.0), MaturityLevel::Inexistent);
        assert_eq!(MaturityLevel::from_score(20.0), MaturityLevel::Prototype);
        assert_eq!(MaturityLevel::from_score(40.0), MaturityLevel::Functional);
        assert_eq!(MaturityLevel::from_score(60.0), MaturityLevel::Validated);
        assert_eq!(MaturityLevel::from_score(80.0), MaturityLevel::Production);
        assert_eq!(MaturityLevel::from_score(95.0), MaturityLevel::Sovereign);
    }

    #[test]
    fn test_maturity_level_description() {
        let level = MaturityLevel::Validated;
        assert!(level.description().contains("Validado"));
    }
}
