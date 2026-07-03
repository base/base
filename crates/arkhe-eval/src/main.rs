//! ARKHE-EVAL CLI


use arkhe_eval::{EvalEngine, EvalConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = EvalConfig::default();
    let engine = EvalEngine::new(config);

    let rt = tokio::runtime::Runtime::new()?;
    let report = rt.block_on(engine.evaluate())?;

    println!("{}", report.to_markdown());

    Ok(())
}
