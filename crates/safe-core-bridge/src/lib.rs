//! Safe-Core Bridge — Integration API.
//!
//! Expõe os serviços do Safe-Core (Ethics, Monitoramento, Invariantes)
//! via HTTP REST (programático) e MCP (coding agents).
//!
//! # Endpoints REST
//!
//! | Método | Rota | Descrição |
//! |--------|------|-----------|
//! | POST | `/api/v1/ethics/enforce` | Verificar ação ética |
//! | GET  | `/api/v1/ethics/violations` | Listar violações |
//! | POST | `/api/v1/ethics/violations` | Limpar violações |
//! | GET  | `/api/v1/invariants` | Listar invariantes |
//! | POST | `/api/v1/invariants/export` | Exportar especificações Lean 4 |
//!
//! # Exemplo de uso (curl)
//!
//! # Verificar se uma ação é permitida (HTTP)
//! curl -X POST http://localhost:8081/api/v1/ethics/enforce \
//!   -H "Content-Type: application/json" \
//!   -d '{"action":"deploy_model","context":{"harm_to_humans":false,"transparent":true}}'
//!
//! # Mesma verificação via MCP (Claude Code, Codex, etc.)
//! # O agente descobre as ferramentas automaticamente via MCP
//!
//! # Verificar health (HTTP)
//! curl http://localhost:8081/health
//!
//! # Verificar health (MCP — o agente faz isso automaticamente)
//! # Ferramenta: health → retorna status dos componentes
//!
//! # Listar invariantes
//! curl http://localhost:8081/api/v1/invariants

pub mod api;
pub mod handlers;
pub mod mcp;
pub mod state;
pub mod tools;

pub use mcp::mcp_impl::SafeCoreMcpServer;
pub use state::BridgeState;
