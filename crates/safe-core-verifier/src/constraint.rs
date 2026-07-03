use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraint {
    pub id: String,
    pub expression: String, // Lean4 expression
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ConstraintResult {
    pub valid: bool,
    pub counterexample: Option<String>,
    pub proof: Option<String>,
}
