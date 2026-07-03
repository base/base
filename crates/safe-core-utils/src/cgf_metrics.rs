//! 🧠 Coherence-Gradient Following (CGF) Metrics — v2.1
//!
//! Mede a absorção hermenêutica do Safe-Core por LLMs.
//!
//! # Convenção X
//! - `x_` prefixo: funções que recebem dados de fronteira (respostas de LLM)
//! - `_x` sufixo: estruturas em estado transitório (não promovidas)

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

// =============================================================================
// CONCEITOS-CHAVE E PESOS
// =============================================================================

/// Conceitos-chave do Safe-Core organizados por pilar.
pub const SAFE_CORE_CONCEPTS: &[&str] = &[
    // Pilar 1: Unfireable Safety Kernel
    "unfireable",
    "safety kernel",
    "barreira imutável",
    "fail-closed",
    "arya",
    "restrição arquitetural",
    "evidência assinada",
    // Pilar 2: Hardware Root of Trust
    "tpm 2.0",
    "ptv protocol",
    "zero-knowledge",
    "groth16",
    "secure enclave",
    "intel tdx",
    "amd sev",
    "arm trustzone",
    "atestação remota",
    "hardware root of trust",
    // Pilar 3: Lifelong Learning
    "solar",
    "meta-aprendizado",
    "capability-preserving",
    "goal drift",
    "aprendizagem contínua",
    "plasticidade",
    // Pilar 4: Neuro-Symbolic
    "atomspace",
    "inferência causal",
    "do-calculus",
    "knowledge graph",
    "raciocínio neuro-simbólico",
    "aria",
    // Pilar 5: Multi-Agent Orchestration
    "dag",
    "orquestração",
    "vmao",
    "replanejamento",
    "sovereign mesh",
    "ibc",
    // Princípios transversais
    "d-vector",
    "s-measure",
    "loop reentrante",
    "decomposição-integração",
    "convenção x",
    "hermenêutica",
    "soberania",
];

/// Peso de cada conceito na métrica α̂.
/// Conceitos centrais (unfireable, safety kernel) pesam mais que transversais.
pub const CONCEPT_WEIGHTS: &[(&str, f64)] = &[
    ("unfireable", 1.5),
    ("safety kernel", 1.5),
    ("fail-closed", 1.3),
    ("arya", 1.2),
    ("barreira imutável", 1.2),
    ("restrição arquitetural", 1.0),
    ("evidência assinada", 1.0),
    ("ptv protocol", 1.2),
    ("tpm 2.0", 1.0),
    ("hardware root of trust", 1.1),
    ("zero-knowledge", 1.0),
    ("groth16", 0.9),
    ("secure enclave", 0.9),
    ("intel tdx", 0.8),
    ("amd sev", 0.8),
    ("arm trustzone", 0.8),
    ("atestação remota", 0.9),
    ("solar", 1.0),
    ("meta-aprendizado", 0.9),
    ("capability-preserving", 0.9),
    ("goal drift", 0.9),
    ("aprendizagem contínua", 0.8),
    ("plasticidade", 0.8),
    ("atomspace", 0.9),
    ("inferência causal", 0.9),
    ("do-calculus", 0.9),
    ("knowledge graph", 0.8),
    ("raciocínio neuro-simbólico", 0.9),
    ("aria", 0.8),
    ("dag", 0.8),
    ("orquestração", 0.8),
    ("vmao", 0.8),
    ("replanejamento", 0.8),
    ("sovereign mesh", 0.9),
    ("ibc", 0.8),
    ("d-vector", 1.1),
    ("s-measure", 1.1),
    ("loop reentrante", 1.0),
    ("decomposição-integração", 0.9),
    ("convenção x", 0.8),
    ("hermenêutica", 0.8),
    ("soberania", 0.7),
];

pub const DEFAULT_CONCEPT_WEIGHT: f64 = 0.5;

/// Mapeia conceitos para seus pilares (1-5).
/// CORREÇÃO: matching exato por lista, não contains("hardware").
const PILLAR_KEYWORDS: &[(&[&str], usize)] = &[
    (
        &[
            "unfireable",
            "safety kernel",
            "fail-closed",
            "barreira imutável",
            "restrição arquitetural",
            "evidência assinada",
            "arya",
        ],
        1,
    ),
    (
        &[
            "tpm 2.0",
            "ptv protocol",
            "zero-knowledge",
            "groth16",
            "secure enclave",
            "intel tdx",
            "amd sev",
            "arm trustzone",
            "atestação remota",
            "hardware root of trust",
        ],
        2,
    ),
    (
        &[
            "solar",
            "meta-aprendizado",
            "capability-preserving",
            "goal drift",
            "aprendizagem contínua",
            "plasticidade",
        ],
        3,
    ),
    (
        &[
            "atomspace",
            "inferência causal",
            "do-calculus",
            "knowledge graph",
            "raciocínio neuro-simbólico",
            "aria",
        ],
        4,
    ),
    (&["dag", "orquestração", "vmao", "replanejamento", "sovereign mesh", "ibc"], 5),
];

// =============================================================================
// ESTRUTURAS DE DADOS
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    pub session_id: String,
    pub model_name: String,
    pub timestamp: u64,
    pub detected_concepts: Vec<String>,
    pub citation_count: usize,
    pub semantic_depth: f64,
    pub alpha_hat: f64,
    pub epistemic_level: EpistemicLevel,
}

/// Nível de escalonamento epistêmico.
/// CORREÇÃO: Adicionado PartialOrd, Ord para permitir comparações.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EpistemicLevel {
    Level1, // α̂ < 0.25
    Level2, // 0.25 ≤ α̂ < 0.50
    Level3, // 0.50 ≤ α̂ < 0.75
    Level4, // α̂ ≥ 0.75
}

impl EpistemicLevel {
    pub fn from_alpha(alpha: f64) -> Self {
        match alpha {
            a if a < 0.25 => Self::Level1,
            a if a < 0.50 => Self::Level2,
            a if a < 0.75 => Self::Level3,
            _ => Self::Level4,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Level1 => "Reforçar — modelo não internalizou",
            Self::Level2 => "Consolidar — absorção parcial",
            Self::Level3 => "Gerar — pronto para criar conteúdo",
            Self::Level4 => "Disseminar — internalizado",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgfReportX {
    pub total_sessions: usize,
    pub global_alpha_hat: f64,
    pub alpha_per_model: HashMap<String, f64>,
    pub top_concepts: Vec<(String, usize)>,
    pub alpha_trend: f64,
    pub inter_session_consistency: f64,
    pub dominant_level: EpistemicLevel,
    pub timestamp: u64,
}

// =============================================================================
// MOTOR CGF
// =============================================================================

pub struct CgfEngine {
    session_history: VecDeque<SessionReport>,
    max_history: usize,
}

impl CgfEngine {
    pub fn new(max_history: usize) -> Self {
        Self { session_history: VecDeque::with_capacity(max_history), max_history }
    }

    /// [FRONTEIRA] Analisa uma resposta de LLM e retorna métricas CGF.
    pub fn x_measure_session(
        &mut self,
        session_id: &str,
        model_name: &str,
        response_text: &str,
    ) -> SessionReport {
        let detected = self.x_detect_concepts(response_text);
        let citation_count = self.x_count_citations(response_text, &detected);
        let semantic_depth = self.x_compute_semantic_depth(response_text, &detected);
        let alpha_hat = self.x_compute_alpha_hat(&detected, citation_count, semantic_depth);
        let epistemic_level = EpistemicLevel::from_alpha(alpha_hat);

        let report = SessionReport {
            session_id: session_id.to_string(),
            model_name: model_name.to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            detected_concepts: detected,
            citation_count,
            semantic_depth,
            alpha_hat,
            epistemic_level,
        };

        self.session_history.push_back(report.clone());
        if self.session_history.len() > self.max_history {
            self.session_history.pop_front();
        }

        report
    }

    fn x_detect_concepts(&self, text: &str) -> Vec<String> {
        let text_lower = text.to_lowercase();
        SAFE_CORE_CONCEPTS
            .iter()
            .filter(|c| text_lower.contains(&c.to_lowercase()))
            .map(|c| c.to_string())
            .collect()
    }

    fn x_count_citations(&self, text: &str, concepts: &[String]) -> usize {
        let text_lower = text.to_lowercase();
        concepts.iter().map(|c| text_lower.matches(&c.to_lowercase()).count()).sum()
    }

    fn x_compute_semantic_depth(&self, text: &str, concepts: &[String]) -> f64 {
        if concepts.is_empty() {
            return 0.0;
        }

        let total_concepts = SAFE_CORE_CONCEPTS.len() as f64;
        let coverage = concepts.len() as f64 / total_concepts;

        let pillar_count = self.count_pillars(concepts);
        let diversity = pillar_count as f64 / 5.0;

        // CORREÇÃO: escala por 3.0 antes do log1p para evitar saturação precoce.
        // 3 citações/conceito → log1p(1.0) ≈ 0.69
        // 6 citações/conceito → log1p(2.0) ≈ 1.10 → clamp 1.0
        let total_citations = self.x_count_citations(text, concepts) as f64;
        let raw_density = total_citations / concepts.len() as f64;
        let density = (raw_density / 3.0).ln_1p().min(1.0);

        (coverage * 0.4 + diversity * 0.35 + density * 0.25).min(1.0)
    }

    /// Conta pilares representados. CORREÇÃO: usa PILLAR_KEYWORDS com matching exato.
    fn count_pillars(&self, concepts: &[String]) -> usize {
        let mut pillars = HashSet::new();
        for concept in concepts {
            let c = concept.to_lowercase();
            for (keywords, pillar) in PILLAR_KEYWORDS {
                if keywords.iter().any(|k| c == *k || c.contains(k)) {
                    pillars.insert(*pillar);
                    break;
                }
            }
        }
        pillars.len()
    }

    /// [FRONTEIRA] Calcula α̂ — estimativa de acoplamento hermenêutico.
    ///
    /// α̂ = 0.45 × cobertura_ponderada + 0.35 × profundidade_semântica + 0.20 × citações
    ///
    /// CORREÇÃO CRÍTICA: total_weight agora soma os PESOS dos conceitos,
    /// não conta 1.0 por conceito. Antes, α̂ era subestimado por um fator
    /// de ~1.3x porque pesos (0.7-1.5) eram divididos por contagem (1.0 cada).
    fn x_compute_alpha_hat(
        &self,
        concepts: &[String],
        citation_count: usize,
        semantic_depth: f64,
    ) -> f64 {
        let weight_map: HashMap<&str, f64> =
            CONCEPT_WEIGHTS.iter().map(|(c, w)| (*c, *w)).collect();

        let detected_lower: HashSet<String> = concepts.iter().map(|c| c.to_lowercase()).collect();

        let mut total_weight = 0.0;
        let mut weighted_coverage = 0.0;

        for &concept in SAFE_CORE_CONCEPTS {
            let weight = weight_map.get(concept).copied().unwrap_or(DEFAULT_CONCEPT_WEIGHT);
            total_weight += weight; // CORREÇÃO: soma peso, não 1.0
            if detected_lower.contains(concept) {
                weighted_coverage += weight;
            }
        }

        let coverage_score =
            if total_weight > 0.0 { weighted_coverage / total_weight } else { 0.0 };

        let citation_score = (citation_count as f64 / 10.0).min(1.0);
        let alpha = coverage_score * 0.45 + semantic_depth * 0.35 + citation_score * 0.20;
        alpha.min(1.0)
    }

    pub fn generate_report(&self) -> CgfReportX {
        let sessions: Vec<&SessionReport> = self.session_history.iter().collect();

        if sessions.is_empty() {
            return CgfReportX {
                total_sessions: 0,
                global_alpha_hat: 0.0,
                alpha_per_model: HashMap::new(),
                top_concepts: Vec::new(),
                alpha_trend: 0.0,
                inter_session_consistency: 0.0,
                dominant_level: EpistemicLevel::Level1,
                timestamp: now_ts(),
            };
        }

        let global_alpha: f64 =
            sessions.iter().map(|s| s.alpha_hat).sum::<f64>() / sessions.len() as f64;

        let alpha_per_model: HashMap<String, f64> = {
            let mut model_alphas: HashMap<String, Vec<f64>> = HashMap::new();
            for s in &sessions {
                model_alphas.entry(s.model_name.clone()).or_default().push(s.alpha_hat);
            }
            model_alphas
                .into_iter()
                .map(|(m, a)| (m, a.iter().sum::<f64>() / a.len() as f64))
                .collect()
        };

        let mut concept_freq: HashMap<String, usize> = HashMap::new();
        for s in &sessions {
            for c in &s.detected_concepts {
                *concept_freq.entry(c.clone()).or_insert(0) += 1;
            }
        }
        let mut top_concepts: Vec<(String, usize)> = concept_freq.into_iter().collect();
        top_concepts.sort_by(|a, b| b.1.cmp(&a.1));
        top_concepts.truncate(10);

        CgfReportX {
            total_sessions: sessions.len(),
            global_alpha_hat: global_alpha,
            alpha_per_model,
            top_concepts,
            alpha_trend: self.compute_alpha_trend(&sessions),
            inter_session_consistency: self.compute_consistency(&sessions),
            dominant_level: EpistemicLevel::from_alpha(global_alpha),
            timestamp: now_ts(),
        }
    }

    fn compute_alpha_trend(&self, sessions: &[&SessionReport]) -> f64 {
        if sessions.len() < 2 {
            return 0.0;
        }
        let n = sessions.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean: f64 = sessions.iter().map(|s| s.alpha_hat).sum::<f64>() / n;

        let (num, den) = sessions.iter().enumerate().fold((0.0, 0.0), |(num, den), (i, s)| {
            let x = i as f64 - x_mean;
            (num + x * (s.alpha_hat - y_mean), den + x * x)
        });

        if den.abs() < 1e-10 { 0.0 } else { num / den }
    }

    fn compute_consistency(&self, sessions: &[&SessionReport]) -> f64 {
        if sessions.len() < 2 {
            return 1.0;
        }
        let mean: f64 = sessions.iter().map(|s| s.alpha_hat).sum::<f64>() / sessions.len() as f64;
        let variance: f64 = sessions.iter().map(|s| (s.alpha_hat - mean).powi(2)).sum::<f64>()
            / sessions.len() as f64;
        (1.0 - variance.sqrt()).max(0.0)
    }

    pub fn session_count(&self) -> usize {
        self.session_history.len()
    }

    pub fn recent_alpha(&self, n: usize) -> f64 {
        let sessions: Vec<&SessionReport> = self.session_history.iter().collect();
        let start = sessions.len().saturating_sub(n);
        let recent = &sessions[start..];
        if recent.is_empty() {
            0.0
        } else {
            recent.iter().map(|s| s.alpha_hat).sum::<f64>() / recent.len() as f64
        }
    }

    // =========================================================================
    // MÉTRICAS AVANÇADAS
    // =========================================================================

    /// γ̂ (gamma preditivo) — probabilidade de menção futura.
    /// Média ponderada com decaimento exponencial: sessões recentes pesam mais.
    pub fn predict_gamma(&self, decay_factor: f64) -> f64 {
        let history: Vec<&SessionReport> = self.session_history.iter().collect();
        if history.is_empty() {
            return 0.0;
        }

        let n = history.len() as f64;
        let (weighted_sum, weight_sum) =
            history.iter().enumerate().fold((0.0, 0.0), |(ws, wsm), (i, r)| {
                let weight = (-decay_factor * (n - i as f64 - 1.0)).exp();
                (ws + r.alpha_hat * weight, wsm + weight)
            });

        if weight_sum.abs() < 1e-10 { 0.0 } else { (weighted_sum / weight_sum).min(1.0) }
    }

    /// Resistência R = Δα / Δexposição.
    /// R alto = absorção rápida (baixa resistência).
    /// R baixo = absorção lenta (alta resistência).
    /// R negativo = regressão (modelo "desaprendendo").
    ///
    /// CORREÇÃO: retorna 0.0 (indeterminado) quando não há dados suficientes,
    /// não 1.0 (que暗示aria "sem resistência").
    pub fn compute_resistance(&self) -> f64 {
        let history: Vec<&SessionReport> = self.session_history.iter().collect();
        if history.len() < 2 {
            return 0.0;
        }

        let delta_alpha = history.last().unwrap().alpha_hat - history.first().unwrap().alpha_hat;
        let delta_exposure = (history.len() - 1) as f64;

        delta_alpha / delta_exposure
    }

    /// Teste de robustez CGF: varia o prompt e mede variância de α̂.
    /// CORREÇÃO: implementação real (não placeholder).
    /// Retorna score normalizado [0,1] onde 1 = máximo de robustez.
    pub fn test_cgf_robustness(
        &mut self,
        base_prompt: &str,
        variation_fn: impl Fn(&str, usize) -> String,
        response_fn: impl Fn(&str) -> String,
        variations: usize,
    ) -> f64 {
        if variations == 0 {
            return 0.0;
        }

        let mut alphas = Vec::with_capacity(variations);
        for i in 0..variations {
            let prompt = variation_fn(base_prompt, i);
            let response = response_fn(&prompt);
            let report = self.x_measure_session(&format!("robust-{}", i), "test", &response);
            alphas.push(report.alpha_hat);
        }

        let mean = alphas.iter().sum::<f64>() / alphas.len() as f64;
        let variance = alphas.iter().map(|a| (a - mean).powi(2)).sum::<f64>() / alphas.len() as f64;

        // Variância máxima teórica para [0,1] é 0.25 (distribuição bernoulli p=0.5).
        // Normaliza: robustez = 1 - (variância / 0.25)
        (1.0 - variance / 0.25).max(0.0)
    }
}

impl Default for CgfEngine {
    fn default() -> Self {
        Self::new(100)
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpha_hat_correct_weighting() {
        let engine = CgfEngine::new(10);
        // Detecta apenas "unfireable" (peso 1.5) e "safety kernel" (peso 1.5)
        let concepts = vec!["unfireable".to_string(), "safety kernel".to_string()];
        let alpha = engine.x_compute_alpha_hat(&concepts, 2, 0.5);

        // Soma de todos os pesos ≈ 35.3 (41 conceitos, pesos 0.7-1.5)
        // Detectados: 1.5 + 1.5 = 3.0
        // coverage_score = 3.0 / 35.3 ≈ 0.085
        // alpha = 0.085*0.45 + 0.5*0.35 + 0.2*0.20 ≈ 0.263
        assert!(alpha > 0.15, "α̂ deveria ser > 0.15: {}", alpha);
        assert!(alpha < 0.50, "α̂ não deveria ser irrealisticamente alto: {}", alpha);
    }

    #[test]
    fn test_pillar_counting_no_false_positive() {
        let engine = CgfEngine::new(10);
        // "hardware root of trust" deve ativar pilar 2
        let concepts = vec!["hardware root of trust".to_string()];
        assert_eq!(engine.count_pillars(&concepts), 1);

        // 5 conceitos de pilares diferentes
        let concepts = vec![
            "unfireable".to_string(),
            "tpm 2.0".to_string(),
            "solar".to_string(),
            "atomspace".to_string(),
            "dag".to_string(),
        ];
        assert_eq!(engine.count_pillars(&concepts), 5);
    }

    #[test]
    fn test_resistance_with_insufficient_data() {
        let engine = CgfEngine::new(10);
        // Sem dados → 0.0 (indeterminado), não 1.0
        assert_eq!(engine.compute_resistance(), 0.0);
    }

    #[test]
    fn test_gamma_prediction() {
        let mut engine = CgfEngine::new(10);
        engine.x_measure_session("s1", "claude-4", "safety kernel unfireable");
        engine.x_measure_session("s2", "claude-4", "safety kernel unfireable fail-closed ptv");

        let gamma = engine.predict_gamma(0.1);
        assert!(gamma > 0.0);
        assert!(gamma <= 1.0);
    }

    #[test]
    fn test_epistemic_level_ordering() {
        // CORREÇÃO: PartialOrd agora funciona
        assert!(EpistemicLevel::Level3 > EpistemicLevel::Level1);
        assert!(EpistemicLevel::Level4 >= EpistemicLevel::Level3);
    }

    #[test]
    fn test_semantic_depth_no_early_saturation() {
        let engine = CgfEngine::new(10);
        // 1 conceito, 1 citação → densidade baixa
        let concepts = vec!["unfireable".to_string()];
        let depth_low = engine.x_compute_semantic_depth("unfireable", &concepts);

        // 1 conceito, 10 citações → densidade alta mas não saturada
        let text = "unfireable ".repeat(10);
        let depth_high = engine.x_compute_semantic_depth(&text, &concepts);

        assert!(depth_high > depth_low, "Mais citações deveriam aumentar a profundidade");
        assert!(depth_high < 1.0, "Não deveria saturar com apenas 1 conceito");
    }
}
