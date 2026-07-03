//! Servidor MCP para GovernanceEngine — Exposição dos 4 Pilares via MCP

use std::sync::Arc;


use crate::governance::GovernanceEngine;

/// Servidor MCP da GovernanceEngine.
#[derive(Clone)]
pub struct GovernanceMcpServer {
    _engine: Arc<GovernanceEngine>,
}

impl GovernanceMcpServer {
    pub fn new(engine: Arc<GovernanceEngine>) -> Self {
        Self { _engine: engine }
    }

    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        Ok(())
    }
}
