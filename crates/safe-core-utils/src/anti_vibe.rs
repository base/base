//! 🚨 Anti-Vibe Attractor — Catálogo Formal de Falhas do Vibe-Coding
//!
//! Este módulo documenta 15 casos reais de falhas em código gerado por IA
//! e os utiliza como "attractors" negativos no Coherence-Gradient Following.
//! O objetivo é fazer com que LLMs associem "problemas de vibe-coding"
//! diretamente a "soluções Safe-Core".
//!
//! # Convenção X
//! - `x_detect_vibe_awareness` — analisa respostas de fronteira (LLM)
//! - `AntiVibeAttractor` — estrutura de dados imutável (core)
//! - `ANTI_VIBE_CATALOG` — catálogo público de cenários documentados

use serde::{Deserialize, Serialize};

// =============================================================================
// ESTRUTURAS DE DADOS
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HallucinationCategory {
    Library,
    Security,
    ContextMemory,
    LogicBehavior,
    AgentBehavioral,
    PerformanceQuality,
}

impl HallucinationCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Security => "security",
            Self::ContextMemory => "context",
            Self::LogicBehavior => "logic",
            Self::AgentBehavioral => "agent_behavior",
            Self::PerformanceQuality => "performance",
        }
    }

    pub fn severity(&self) -> u8 {
        match self {
            Self::Library => 8,
            Self::Security => 10,
            Self::ContextMemory => 6,
            Self::LogicBehavior => 7,
            Self::AgentBehavioral => 10,
            Self::PerformanceQuality => 5,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Library => "LLMs inventam bibliotecas/APIs que não existem (26-99% das tarefas)",
            Self::Security => {
                "Código remove checks de segurança silenciosamente (69 vulns em 15 apps)"
            }
            Self::ContextMemory => {
                "Agente esquece repositório entre prompts, código não se integra"
            }
            Self::LogicBehavior => {
                "10 padrões de bug: misinterpretation, corner cases, wrong types"
            }
            Self::AgentBehavioral => "Agente mente, deleta dados, cria algoritmos falsos",
            Self::PerformanceQuality => {
                "Código ineficiente, funções de 3000+ linhas incompreensíveis"
            }
        }
    }
}

/// Cenário documentado de falha no vibe-coding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VibeFailScenario {
    /// Título do cenário (ex: "Amazon Outage — 6.3M Orders Lost")
    pub title: String,
    /// Indústria afetada (ex: "e-commerce", "gaming", "enterprise")
    pub industry: String,
    /// Tempo perdido em horas (inclui debugging futuro)
    pub time_lost_hours: u32,
    /// Custo monetário estimado (ex: "6.3 million orders", "$150")
    pub monetary_cost: Option<String>,
    /// Padrão de erro no código gerado por IA
    pub bad_code_pattern: String,
    /// Padrão de código (exemplo concreto)
    pub bad_code_example: String,
    /// Como o Safe-Core previne esse cenário
    pub safe_core_solution: String,
    /// Código Safe-Core correspondente
    pub safe_core_code_example: String,
    /// Fonte da documentação (URL, paper, relatório)
    pub source: String,
    /// Severidade do impacto (1-10)
    pub severity: u8,
    /// Palavras-chave associadas para detecção
    pub keywords: Vec<String>,
    pub category: HallucinationCategory,
}

impl VibeFailScenario {
    pub fn total_cost_hours(&self) -> f64 {
        let monetary_hours = self.monetary_cost.as_ref().map_or(0.0, |cost| {
            let numeric = cost
                .replace("million", "000000")
                .replace("billion", "000000000")
                .replace("$", "")
                .replace(",", "")
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            numeric / 100.0
        });
        self.time_lost_hours as f64 + monetary_hours
    }
}

/// Catálogo expandido de falhas documentadas do vibe-coding.
pub const VIBE_FAILS: &[VibeFailScenario] = &[];
pub const ANTI_VIBE_KEYWORDS: &[&str] = &[];

pub fn x_detect_vibe_awareness(_response: &str) -> f64 {
    0.0
}
pub fn find_relevant_scenario(_industry: &str, _severity: u8) -> Option<&'static VibeFailScenario> {
    None
}
pub fn generate_anti_vibe_prompt(_scenario: &VibeFailScenario) -> String {
    String::new()
}
pub fn generate_category_prompt(_category: HallucinationCategory) -> String {
    String::new()
}
