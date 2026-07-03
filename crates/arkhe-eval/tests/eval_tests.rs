use arkhe_eval::{EvalConfig, EvalEngine};

#[tokio::test]
async fn test_engine_creation() {
    let config = EvalConfig::default();
    let _engine = EvalEngine::new(config);
    // Just verify it creates
}

#[tokio::test]
async fn test_dimensions_have_correct_weights() {
    let dimensions = EvalEngine::build_dimensions();
    let total_weight: f64 = dimensions.iter().map(|d| d.weight).sum();
    assert!((total_weight - 1.0).abs() < 0.001);
}
