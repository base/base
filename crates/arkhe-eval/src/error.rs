//! Tipos de erro para o framework de avaliação

use thiserror::Error;

#[derive(Error, Debug)]
pub enum EvalError {
    #[error("Workspace not found: {0}")]
    WorkspaceNotFound(String),

    #[error("Dimension error: {0}")]
    DimensionError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, EvalError>;
