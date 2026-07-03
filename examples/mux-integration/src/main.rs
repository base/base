//! Exemplo de integração do Safe-Core Governance com multiplexadores
//!
//! Este exemplo mostra como o Safe-Core pode ser integrado com qualquer
//! multiplexador (*mux) que suporte MCP.

use safe_core_governance::{GovernanceEngine, GovernanceMcpServer};
use std::sync::Arc;
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,safe_core_governance=debug")
        .init();

    // 1. Inicializa a Engine de Governança
    let engine = Arc::new(GovernanceEngine::new("sqlite://state.db").await?);

    // 2. Carrega regras iniciais
    {
        use safe_core_governance::ethics::{EthicsRule, Severity};
        let rules = vec![
            EthicsRule {
                id: "rule_001".to_string(),
                action: "increase_price".to_string(),
                constraint: "context.percentage <= 20".to_string(),
                severity: Severity::Block,
                enabled: true,
            },
            EthicsRule {
                id: "rule_002".to_string(),
                action: "approve_loan".to_string(),
                constraint: "context.amount <= 100000".to_string(),
                severity: Severity::RequireApproval,
                enabled: true,
            },
        ];
        engine.repository.save_rules(&rules).await?;
        engine.ethics.write().await.load_rules(rules).await?;
    }

    // 3. Inicia o servidor MCP
    let server = GovernanceMcpServer::new(engine);
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.serve_stdio().await {
            eprintln!("Erro no servidor MCP: {}", e);
        }
    });

    // 4. Aguarda sinal de desligamento
    signal::ctrl_c().await?;
    println!("Desligando...");

    server_handle.abort();
    Ok(())
}
