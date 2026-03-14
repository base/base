mod errors;
pub use errors::{BaselineError, Result};

mod logging;
pub use logging::{TracingGuard, init_tracing};
