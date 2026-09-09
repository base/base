//! Primitive types for `base-consensus-derive-pipeline`.

mod results;
pub use results::{PipelineResult, StepResult};

mod signals;
pub use signals::{ActivationSignal, ResetSignal, Signal};
