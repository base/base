//! ARKHE Evaluation Framework
//!
//! Avalia a maturidade do ARKHE OS através de 7 dimensões:
//! 1. Arquitetura Conceitual (15%)
//! 2. Especificação Formal (20%)
//! 3. Compilabilidade (20%)
//! 4. Testabilidade (15%)
//! 5. Integração (15%)
//! 6. Documentação (5%)
//! 7. Maturidade Operacional (10%)

pub mod dimensions;
pub mod engine;
pub mod report;
pub mod error;

pub use dimensions::{Dimension, DimensionScore, Criterion, CriterionResult};
pub use engine::{EvalEngine, EvalConfig};
pub use report::{EvalReport, MaturityLevel};
pub use error::{EvalError, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
