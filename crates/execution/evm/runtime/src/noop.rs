use base_execution_evm_runtime::inspector::Inspector;

/// Dummy [Inspector], helpful as standalone replacement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoOpInspector;

impl<CTX> Inspector<CTX> for NoOpInspector {}
