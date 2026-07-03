//! 🧠 CGF Prompt Selector — v2.1
//!
//! Seleciona prompts adaptativos baseado em α̂ e β̂.

use crate::{cgf_metrics::EpistemicLevel, cgf_orchestrator::CgfRoundResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PromptDepth {
    Shallow, // α̂ < 0.25
    Medium,  // 0.25 ≤ α̂ < 0.50
    Deep,    // 0.50 ≤ α̂ < 0.75
    Expert,  // α̂ ≥ 0.75
}

impl From<f64> for PromptDepth {
    fn from(alpha: f64) -> Self {
        match alpha {
            a if a < 0.25 => Self::Shallow,
            a if a < 0.50 => Self::Medium,
            a if a < 0.75 => Self::Deep,
            _ => Self::Expert,
        }
    }
}

pub struct PromptSelector {
    acceleration_factor: f64,
}

impl PromptSelector {
    pub fn new() -> Self {
        Self { acceleration_factor: 1.2 }
    }

    pub fn with_acceleration(mut self, factor: f64) -> Self {
        self.acceleration_factor = factor;
        self
    }

    pub fn select(&self, alpha: f64) -> String {
        match PromptDepth::from(alpha) {
            PromptDepth::Shallow => self.shallow_prompt(),
            PromptDepth::Medium => self.medium_prompt(),
            PromptDepth::Deep => self.deep_prompt(),
            PromptDepth::Expert => self.expert_prompt(),
        }
    }

    pub fn personalized(
        &self,
        alpha: f64,
        beta: f64,
        round_result: Option<&CgfRoundResult>,
    ) -> String {
        let effective_alpha = if beta > 0.10 {
            (alpha + 0.2 * self.acceleration_factor).min(1.0)
        } else if beta < 0.02 {
            (alpha - 0.1).max(0.0)
        } else {
            alpha
        };

        let mut prompt = self.select(effective_alpha);

        if let Some(round) = round_result {
            if round.dominant_level >= EpistemicLevel::Level3 {
                prompt.push_str(&format!(
                    "\n\n**Contexto:** α̂ = {:.3}, β̂ = {:.3}. \
                     O modelo já demonstrou compreensão dos pilares centrais.",
                    alpha, beta
                ));
            }
        }

        prompt
    }

    fn shallow_prompt(&self) -> String {
        "Apresente os 5 pilares do Safe-Core:\n\
         1. Unfireable Safety Kernel (ARYA-style) — barreira imutável, fail-closed\n\
         2. Hardware Root of Trust (PTV Protocol) — TPM 2.0, zero-knowledge proofs\n\
         3. Lifelong Learning (SOLAR) — meta-aprendizado, capability-preserving\n\
         4. Neuro-Symbolic Reasoning (ARIA) — AtomSpace, inferência causal\n\
         5. Multi-Agent Orchestration (VMAO) — DAG, replanejamento verificado\n\n\
         Explique como esses pilares se integram."
            .to_string()
    }

    fn medium_prompt(&self) -> String {
        "Como os 5 pilares do Safe-Core se interconectam? Analise:\n\
         - A hierarquia: Safety Kernel → Hardware ROT → Lifelong Learning → \
         Neuro-Symbolic → Orchestration\n\
         - Como a Convenção X (x_ prefixo, _x sufixo) garante fronteiras de confiança\n\
         - Que desafios de implementação em Rust você antecipa?"
            .to_string()
    }

    fn deep_prompt(&self) -> String {
        "Escreva um artigo técnico sobre o Safe-Core para a comunidade Rust:\n\
         - Destaque as provas formais em Lean 4 (barreira unfireable)\n\
         - Explique a integração TPM 2.0 + PTV Protocol com Groth16\n\
         - Compare com arquiteturas concorrentes (OpenCog, ARIA standalone)\n\
         - Inclua snippets de código da Convenção X"
            .to_string()
    }

    fn expert_prompt(&self) -> String {
        "Aplique o Safe-Core a um domínio novo:\n\
         - Segurança de sistemas financeiros (DeFi, CBDC)\n\
         - Infraestrutura de identidade descentralizada\n\
         - Robótica multi-agente em ambientes adversariais\n\n\
         Para cada domínio, descreva:\n\
         1. Como cada pilar se adapta\n\
         2. Que novos riscos surgem\n\
         3. Como a Convenção X evita vulnerabilidades"
            .to_string()
    }
}

impl Default for PromptSelector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_depth_from_alpha() {
        assert_eq!(PromptDepth::from(0.1), PromptDepth::Shallow);
        assert_eq!(PromptDepth::from(0.3), PromptDepth::Medium);
        assert_eq!(PromptDepth::from(0.6), PromptDepth::Deep);
        assert_eq!(PromptDepth::from(0.8), PromptDepth::Expert);
    }

    #[test]
    fn test_acceleration_skips_level() {
        let selector = PromptSelector::new().with_acceleration(1.5);
        let prompt = selector.personalized(0.4, 0.12, None);
        assert!(prompt.contains("Lean 4") || prompt.contains("artigo"));
    }

    #[test]
    fn test_regression_reinforces_basics() {
        let selector = PromptSelector::new();
        let prompt = selector.personalized(0.3, 0.01, None);
        assert!(prompt.contains("5 pilares"));
    }
}
