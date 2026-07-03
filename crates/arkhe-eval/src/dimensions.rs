//! Definições de dimensões e critérios de avaliação

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dimension {
    pub id: String,
    pub name: String,
    pub weight: f64,
    pub criteria: Vec<Criterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionScore {
    pub dimension_id: String,
    pub dimension_name: String,
    pub raw_score: f64,
    pub weighted_score: f64,
    pub criteria_results: Vec<CriterionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub description: String,
    pub max_points: f64,
    pub check_type: CheckType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckType {
    FileExists { path: String },
    DirExists { path: String },
    FileContains { path: String, text: String },
    CommandSuccess { command: String },
    Custom { checker: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion_id: String,
    pub description: String,
    pub points_earned: f64,
    pub max_points: f64,
    pub passed: bool,
    pub evidence: String,
}

impl Dimension {
    pub fn new(id: &str, name: &str, weight: f64) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            weight,
            criteria: Vec::new(),
        }
    }

    pub fn with_criterion(mut self, criterion: Criterion) -> Self {
        self.criteria.push(criterion);
        self
    }
}

impl Criterion {
    pub fn new(id: &str, description: &str, max_points: f64, check_type: CheckType) -> Self {
        Self {
            id: id.to_string(),
            description: description.to_string(),
            max_points,
            check_type,
        }
    }
}

impl DimensionScore {
    pub fn calculate_percentage(&self) -> f64 {
        if self.raw_score == 0.0 {
            return 0.0;
        }
        let total_max: f64 = self.criteria_results.iter().map(|c| c.max_points).sum();
        if total_max == 0.0 {
            return 0.0;
        }
        (self.raw_score / total_max) * 100.0
    }
}
