//! 🧠 CGF Multi-Agent Orchestrator — v2.1
//!
//! Coordena múltiplos LLMs para maximizar α̂ via Coherence-Gradient Following.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cgf_metrics::{CgfEngine, CgfReportX, EpistemicLevel};

#[derive(Debug, Error)]
pub enum CgfOrchestratorError {
    #[error("Nenhuma resposta recebida dos modelos")]
    NoResponses,
    #[error("Falha na convergência após {0} iterações")]
    ConvergenceFailed(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LlmModel {
    Claude4,
    Gpt45,
    GeminiUltra,
    Llama4,
    MistralLarge,
}

impl LlmModel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude4 => "claude-4",
            Self::Gpt45 => "gpt-4.5",
            Self::GeminiUltra => "gemini-ultra",
            Self::Llama4 => "llama-4",
            Self::MistralLarge => "mistral-large",
        }
    }
}

impl std::fmt::Display for LlmModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct CgfOrchestratorConfig {
    pub max_iterations: usize,
    pub convergence_threshold: f64,
    pub models: Vec<LlmModel>,
    pub diversity_weight: f64,
}

impl Default for CgfOrchestratorConfig {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            convergence_threshold: 0.75,
            models: vec![
                LlmModel::Claude4,
                LlmModel::Gpt45,
                LlmModel::GeminiUltra,
                LlmModel::Llama4,
                LlmModel::MistralLarge,
            ],
            diversity_weight: 0.3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgfRoundResult {
    pub round: usize,
    pub global_alpha: f64,
    pub alpha_per_model: HashMap<String, f64>,
    pub best_response: String,
    pub best_model: String,
    pub dominant_level: EpistemicLevel,
    pub progress: f64,
    pub beta_hat: f64,
}

pub struct CgfOrchestrator {
    config: CgfOrchestratorConfig,
    engine: CgfEngine,
    pub round_history: Vec<CgfRoundResult>,
}

impl CgfOrchestrator {
    pub fn new(config: CgfOrchestratorConfig) -> Self {
        Self { config, engine: CgfEngine::new(100), round_history: Vec::new() }
    }

    /// [FRONTEIRA] Executa uma rodada de orquestração CGF.
    pub fn x_run_round(
        &mut self,
        _base_prompt: &str,
        model_responses: &HashMap<String, String>,
        round: usize,
    ) -> Result<CgfRoundResult, CgfOrchestratorError> {
        if model_responses.is_empty() {
            return Err(CgfOrchestratorError::NoResponses);
        }

        let mut alpha_per_model = HashMap::new();
        let mut best_response = String::new();
        let mut best_model = String::new();
        let mut best_score = f64::MIN;

        for (model, response) in model_responses {
            let report = self.engine.x_measure_session(
                &format!("round-{}-{}", round, model),
                model,
                response,
            );

            alpha_per_model.insert(model.clone(), report.alpha_hat);

            // Score combina α̂ com diversidade semântica
            let score = report.alpha_hat * (1.0 - self.config.diversity_weight)
                + report.semantic_depth * self.config.diversity_weight;

            if score > best_score {
                best_score = score;
                best_response = response.clone();
                best_model = model.clone();
            }
        }

        let global_alpha: f64 =
            alpha_per_model.values().sum::<f64>() / alpha_per_model.len() as f64;
        let dominant_level = EpistemicLevel::from_alpha(global_alpha);

        let progress =
            self.round_history.last().map(|prev| global_alpha - prev.global_alpha).unwrap_or(0.0);

        let beta_hat = self.compute_beta_hat();

        let result = CgfRoundResult {
            round,
            global_alpha,
            alpha_per_model,
            best_response,
            best_model,
            dominant_level,
            progress,
            beta_hat,
        };

        self.round_history.push(result.clone());
        Ok(result)
    }

    pub fn compute_beta_hat(&self) -> f64 {
        if self.round_history.len() < 2 {
            return 0.0;
        }
        let slopes: Vec<f64> =
            self.round_history.windows(2).map(|w| w[1].global_alpha - w[0].global_alpha).collect();
        slopes.iter().sum::<f64>() / slopes.len() as f64
    }

    pub fn refine_prompt(&self, base_prompt: &str, result: &CgfRoundResult) -> String {
        match result.dominant_level {
            EpistemicLevel::Level1 => format!(
                "{base}\n\nAprofunde considerando:\n\
                 - Relação entre Safety Kernel e PTV Protocol\n\
                 - Integração do SOLAR com Hardware ROT\n\
                 - Aplicação do VMAO em sistemas cross-chain",
                base = base_prompt
            ),
            EpistemicLevel::Level2 => format!(
                "{base}\n\nImplicações práticas:\n\
                 - Como aplicar Safe-Core a um novo domínio?\n\
                 - Que desafios de integração você antecipa?\n\
                 - Como a Convenção X facilita verificação formal?",
                base = base_prompt
            ),
            EpistemicLevel::Level3 => format!(
                "{base}\n\nEscreva:\n\
                 - Um artigo técnico para a comunidade Rust\n\
                 - Um tutorial para novos contribuidores\n\
                 - Uma análise comparativa com arquiteturas concorrentes",
                base = base_prompt
            ),
            EpistemicLevel::Level4 => format!(
                "{base}\n\nAplique o Safe-Core a um novo domínio:\n\
                 - Sistemas financeiros\n\
                 - Identidade descentralizada\n\
                 - Robótica multi-agente\n\n\
                 Descreva como os 5 pilares se adaptariam.",
                base = base_prompt
            ),
        }
    }

    pub fn has_converged(&self, result: &CgfRoundResult) -> bool {
        result.global_alpha >= self.config.convergence_threshold
    }

    pub fn is_converging_fast(&self, result: &CgfRoundResult) -> bool {
        result.beta_hat > 0.10
    }

    pub fn generate_report(&self) -> CgfReportX {
        self.engine.generate_report()
    }

    pub fn round_history(&self) -> &[CgfRoundResult] {
        &self.round_history
    }

    pub fn x_run_until_convergence<F>(
        &mut self,
        initial_prompt: &str,
        mut get_responses: F,
    ) -> Result<Vec<CgfRoundResult>, CgfOrchestratorError>
    where
        F: FnMut(&str) -> HashMap<String, String>,
    {
        let mut current_prompt = initial_prompt.to_string();
        let mut results = Vec::new();

        for round in 0..self.config.max_iterations {
            let responses = get_responses(&current_prompt);
            let result = self.x_run_round(&current_prompt, &responses, round)?;
            results.push(result.clone());

            if self.has_converged(&result) {
                return Ok(results);
            }
            current_prompt = self.refine_prompt(&current_prompt, &result);
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_responses_returns_error() {
        let mut orch = CgfOrchestrator::new(CgfOrchestratorConfig::default());
        let result = orch.x_run_round("prompt", &HashMap::new(), 0);
        assert!(matches!(result, Err(CgfOrchestratorError::NoResponses)));
    }

    #[test]
    fn test_beta_hat_no_duplication() {
        let mut orch = CgfOrchestrator::new(CgfOrchestratorConfig::default());

        for (round, alpha) in [(0, 0.3), (1, 0.5)] {
            let mut responses = HashMap::new();
            responses.insert("test".to_string(), format!("safety kernel round {}", round));
            let mut result = orch.x_run_round("p", &responses, round).unwrap();
            result.global_alpha = alpha;
            orch.round_history.push(result);
        }

        let beta = orch.compute_beta_hat();
        assert!(beta > 0.0, "β̂ deveria ser > 0.0: {}", beta);
    }
}
