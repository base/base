pub mod cgf_metrics;
pub mod cgf_orchestrator;
pub mod cgf_prompt_selector;

#[cfg(feature = "anti-vibe")]
pub mod anti_vibe;

#[cfg(feature = "anti-vibe")]
pub use anti_vibe::{
    ANTI_VIBE_KEYWORDS, VIBE_FAILS, VibeFailScenario, find_relevant_scenario,
    generate_anti_vibe_prompt, x_detect_vibe_awareness,
};
pub use cgf_metrics::{
    CONCEPT_WEIGHTS, CgfEngine, CgfReportX, EpistemicLevel, SAFE_CORE_CONCEPTS, SessionReport,
};
pub use cgf_orchestrator::{
    CgfOrchestrator, CgfOrchestratorConfig, CgfOrchestratorError, CgfRoundResult, LlmModel,
};
pub use cgf_prompt_selector::{PromptDepth, PromptSelector};
