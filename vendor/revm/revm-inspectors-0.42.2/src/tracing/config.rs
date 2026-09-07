use alloy_primitives::{map::HashSet, U256};
use alloy_rpc_types_trace::{
    geth::{
        erc7562::Erc7562Config, CallConfig, FlatCallConfig, GethDefaultTracingOptions,
        PreStateConfig,
    },
    parity::TraceType,
};
use revm::bytecode::opcode::OpCode;

/// 256 bits each marking whether an opcode should be included into steps trace or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct OpcodeFilter(U256);

impl Default for OpcodeFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpcodeFilter {
    /// Returns a new [OpcodeFilter] that does not trace any opcodes.
    #[inline]
    pub const fn new() -> Self {
        Self(U256::ZERO)
    }

    /// Returns whether steps with given [OpCode] should be traced.
    #[inline]
    pub fn is_enabled(&self, op: OpCode) -> bool {
        self.0.bit(op.get() as usize)
    }

    /// Enables tracing of given [OpCode].
    #[inline]
    pub fn enable(&mut self, op: OpCode) -> &mut Self {
        self.0.set_bit(op.get() as usize, true);
        self
    }

    /// Enables tracing of given [OpCode].
    #[inline]
    pub const fn enabled(mut self, op: OpCode) -> Self {
        let index = op.get() as usize;
        let mut limbs = self.0.into_limbs();
        let (limb, bit) = (index / 64, index % 64);
        limbs[limb] |= 1 << bit;
        self.0 = U256::from_limbs(limbs);
        self
    }
}

/// Gives guidance to the [TracingInspector](crate::tracing::TracingInspector).
///
/// Use [TracingInspectorConfig::default_parity] or [TracingInspectorConfig::default_geth] to get
/// the default configs for specific styles of traces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TracingInspectorConfig {
    /// Whether to record every individual opcode level step.
    pub record_steps: bool,
    /// Whether to record individual memory snapshots.
    pub record_memory_snapshots: bool,
    /// Whether to record individual stack snapshots.
    pub record_stack_snapshots: StackSnapshotType,
    /// Whether to record state diffs.
    pub record_state_diff: bool,
    /// Whether to record returndata buffer snapshots.
    pub record_returndata_snapshots: bool,
    /// Optional filter for opcodes to record. If provided, only steps with opcode in this set will
    /// be recorded.
    pub record_opcodes_filter: Option<OpcodeFilter>,
    /// Whether to ignore precompile calls.
    pub exclude_precompile_calls: bool,
    /// Whether to record logs
    pub record_logs: bool,
    /// Whether to record immediate bytes for opcodes.
    pub record_immediate_bytes: bool,
}

impl TracingInspectorConfig {
    /// Returns a config with everything enabled.
    pub const fn all() -> Self {
        Self {
            record_steps: true,
            record_memory_snapshots: true,
            record_stack_snapshots: StackSnapshotType::All,
            record_state_diff: true,
            record_returndata_snapshots: true,
            record_opcodes_filter: None,
            exclude_precompile_calls: false,
            record_logs: true,
            record_immediate_bytes: true,
        }
    }

    /// Returns a config with everything disabled.
    pub const fn none() -> Self {
        Self {
            record_steps: false,
            record_memory_snapshots: false,
            record_stack_snapshots: StackSnapshotType::None,
            record_state_diff: false,
            record_returndata_snapshots: false,
            exclude_precompile_calls: false,
            record_logs: false,
            record_opcodes_filter: None,
            record_immediate_bytes: false,
        }
    }

    /// Returns a config for parity style traces.
    ///
    /// This config does _not_ record opcode level traces and is suited for `trace_transaction`
    pub const fn default_parity() -> Self {
        Self {
            record_steps: false,
            record_memory_snapshots: false,
            record_stack_snapshots: StackSnapshotType::None,
            record_state_diff: false,
            record_returndata_snapshots: false,
            exclude_precompile_calls: true,
            record_logs: false,
            record_opcodes_filter: None,
            record_immediate_bytes: false,
        }
    }

    /// Returns the [`TracingInspectorConfig`] for [`TraceType::StateDiff`].
    ///
    /// This is the same as [`Self::default_parity`]
    ///
    /// Note: the parity statediffs can be populated entirely via the execution result, so we don't
    /// need statediff recording
    pub const fn parity_statediff() -> Self {
        Self::default_parity()
    }

    /// Returns the [`TracingInspectorConfig`] for [`TraceType::VmTrace`].
    pub const fn parity_vm_trace() -> Self {
        Self::default_parity()
            .set_steps(true)
            .set_stack_snapshots(StackSnapshotType::Pushes)
            .set_memory_snapshots(true)
            // also need statediffs for recording altered storage in `VmExecutedOperation.store`
            .set_state_diffs(true)
    }

    /// Returns a config for geth style traces.
    ///
    /// This config does _not_ record opcode level traces and is suited for `debug_traceTransaction`
    ///
    /// This will configure the default output of geth's default
    /// [StructLogTracer](alloy_rpc_types_trace::geth::DefaultFrame).
    pub const fn default_geth() -> Self {
        Self {
            record_steps: true,
            record_memory_snapshots: false,
            record_stack_snapshots: StackSnapshotType::Full,
            record_state_diff: true,
            record_returndata_snapshots: false,
            exclude_precompile_calls: false,
            record_logs: false,
            record_opcodes_filter: None,
            record_immediate_bytes: false,
        }
    }

    /// Returns the [TracingInspectorConfig] depending on the enabled [TraceType]s
    ///
    /// Note: the parity statediffs can be populated entirely via the execution result, so we don't
    /// need statediff recording
    #[inline]
    pub fn from_parity_config(trace_types: &HashSet<TraceType>) -> Self {
        let needs_vm_trace = trace_types.contains(&TraceType::VmTrace);
        let snap_type =
            if needs_vm_trace { StackSnapshotType::Pushes } else { StackSnapshotType::None };
        Self::default_parity()
            .set_steps(needs_vm_trace)
            .set_stack_snapshots(snap_type)
            .set_memory_snapshots(needs_vm_trace)
    }

    /// Returns a config for geth style traces based on the given [GethDefaultTracingOptions].
    ///
    /// This will configure the output of geth's default
    /// [StructLogTracer](alloy_rpc_types_trace::geth::DefaultFrame) according to the given config.
    #[inline]
    pub fn from_geth_config(config: &GethDefaultTracingOptions) -> Self {
        Self {
            record_memory_snapshots: config.enable_memory.unwrap_or_default(),
            record_stack_snapshots: if config.disable_stack.unwrap_or_default() {
                StackSnapshotType::None
            } else {
                StackSnapshotType::Full
            },
            record_state_diff: !config.disable_storage.unwrap_or_default(),
            record_returndata_snapshots: config.is_return_data_enabled(),
            ..Self::default_geth()
        }
    }

    /// Returns a config for geth's [CallTracer](alloy_rpc_types_trace::geth::CallFrame).
    ///
    /// This returns [Self::none] and enables [TracingInspectorConfig::record_logs] if configured in
    /// the given [CallConfig]
    #[inline]
    pub fn from_geth_call_config(config: &CallConfig) -> Self {
        Self::none()
            // call tracer is similar parity tracer with optional support for logs
            .set_record_logs(config.with_log.unwrap_or_default())
    }

    /// Returns a config for geth's
    /// [Erc7562Frame](alloy_rpc_types_trace::geth::erc7562::Erc7562Frame).
    #[inline]
    pub fn from_geth_erc7562_config(config: &Erc7562Config) -> Self {
        Self::none()
            // call tracer is similar parity tracer with optional support for logs
            .set_record_logs(config.with_log.unwrap_or_default())
            // need memory snapshots for keccak preimages
            .set_memory_snapshots(true)
            // need stack snapshots for keccak preimages
            .set_stack_snapshots(StackSnapshotType::Full)
            // need state diffs to capture SLOAD values in accessedSlots.reads
            .set_state_diffs(true)
            .steps()
    }

    /// Returns a config for geth's
    /// [FlatCallTracer](alloy_rpc_types_trace::geth::call::FlatCallFrame).
    ///
    /// This returns [Self::default_parity] and sets
    /// [TracingInspectorConfig::exclude_precompile_calls] if configured in the given
    /// [FlatCallConfig]
    #[inline]
    pub fn from_flat_call_config(config: &FlatCallConfig) -> Self {
        Self::default_parity()
            // call tracer is similar parity tracer with optional support for logs
            .set_exclude_precompile_calls(!config.include_precompiles.unwrap_or_default())
    }

    /// Returns a config for geth's [PrestateTracer](alloy_rpc_types_trace::geth::PreStateFrame).
    ///
    /// Note: This currently returns [Self::none] because the prestate tracer result currently
    /// relies on the execution result entirely, see
    /// [GethTraceBuilder::geth_prestate_traces](crate::tracing::geth::GethTraceBuilder::geth_prestate_traces)
    #[inline]
    pub const fn from_geth_prestate_config(_config: &PreStateConfig) -> Self {
        Self::none()
    }

    /// Merge another config into this one.
    #[inline]
    pub fn merge(&mut self, other: Self) -> &mut Self {
        self.record_steps |= other.record_steps;
        self.record_memory_snapshots |= other.record_memory_snapshots;
        self.record_stack_snapshots = other.record_stack_snapshots;
        self.record_state_diff |= other.record_state_diff;
        self.record_returndata_snapshots |= other.record_returndata_snapshots;
        self.exclude_precompile_calls |= other.exclude_precompile_calls;
        self.record_logs |= other.record_logs;
        self.record_opcodes_filter = self.record_opcodes_filter.or(other.record_opcodes_filter);
        self.record_immediate_bytes |= other.record_immediate_bytes;
        self
    }

    /// Configure whether calls to precompiles should be ignored.
    ///
    /// If set to `true`, calls to precompiles without value transfers will be ignored.
    pub const fn set_exclude_precompile_calls(mut self, exclude_precompile_calls: bool) -> Self {
        self.exclude_precompile_calls = exclude_precompile_calls;
        self
    }

    /// Disable recording of individual opcode level steps
    pub const fn disable_steps(self) -> Self {
        self.set_steps(false)
    }

    /// Enable recording of individual opcode level steps
    pub const fn steps(self) -> Self {
        self.set_steps(true)
    }

    /// Configure whether individual opcode level steps should be recorded
    pub const fn set_steps(mut self, record_steps: bool) -> Self {
        self.record_steps = record_steps;
        self
    }

    /// Disable recording of individual memory snapshots
    pub const fn disable_memory_snapshots(self) -> Self {
        self.set_memory_snapshots(false)
    }

    /// Enable recording of individual memory snapshots
    pub const fn memory_snapshots(self) -> Self {
        self.set_memory_snapshots(true)
    }

    /// Configure whether the tracer should record memory snapshots
    pub const fn set_memory_snapshots(mut self, record_memory_snapshots: bool) -> Self {
        self.record_memory_snapshots = record_memory_snapshots;
        self
    }

    /// Disable recording of individual stack snapshots
    pub const fn disable_stack_snapshots(self) -> Self {
        self.set_stack_snapshots(StackSnapshotType::None)
    }

    /// Enable recording of individual stack snapshots
    pub const fn stack_snapshots(self) -> Self {
        self.set_stack_snapshots(StackSnapshotType::Full)
    }

    /// Configure how the tracer should record stack snapshots
    pub const fn set_stack_snapshots(mut self, record_stack_snapshots: StackSnapshotType) -> Self {
        self.record_stack_snapshots = record_stack_snapshots;
        self
    }

    /// Disable recording of state diffs
    pub const fn disable_state_diffs(self) -> Self {
        self.set_state_diffs(false)
    }

    /// Configure whether the tracer should record state diffs
    pub const fn set_state_diffs(mut self, record_state_diff: bool) -> Self {
        self.record_state_diff = record_state_diff;
        self
    }

    /// Sets state diff recording to true.
    ///
    /// Also enables steps recording since state diff recording requires steps recording.
    pub const fn with_state_diffs(self) -> Self {
        self.set_steps_and_state_diffs(true)
    }

    /// Configure whether the tracer should record steps and state diffs.
    ///
    /// This is a convenience method for setting both [TracingInspectorConfig::set_steps] and
    /// [TracingInspectorConfig::set_state_diffs] since tracking state diffs requires steps tracing.
    pub const fn set_steps_and_state_diffs(mut self, steps_and_diffs: bool) -> Self {
        self.record_steps = steps_and_diffs;
        self.record_state_diff = steps_and_diffs;
        self
    }

    /// Disable recording of individual logs
    pub const fn disable_record_logs(self) -> Self {
        self.set_record_logs(false)
    }

    /// Enable recording of individual logs
    pub const fn record_logs(self) -> Self {
        self.set_record_logs(true)
    }

    /// Configure whether the tracer should record logs
    pub const fn set_record_logs(mut self, record_logs: bool) -> Self {
        self.record_logs = record_logs;
        self
    }

    /// Configure whether the tracer should record immediate bytes
    pub const fn set_immediate_bytes(mut self, record_immediate_bytes: bool) -> Self {
        self.record_immediate_bytes = record_immediate_bytes;
        self
    }

    /// Enable recording of immediate bytes
    pub const fn record_immediate_bytes(self) -> Self {
        self.set_immediate_bytes(true)
    }

    /// If [OpcodeFilter] is configured, returns whether the given opcode should be recorded.
    /// Otherwise, always returns true.
    #[inline]
    pub fn should_record_opcode(&self, op: OpCode) -> bool {
        self.record_opcodes_filter.as_ref().is_none_or(|filter| filter.is_enabled(op))
    }
}

/// How much of the stack to record. Nothing, just the items pushed, or the full stack
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StackSnapshotType {
    /// Don't record stack snapshots
    #[default]
    None,
    /// Record full, push stack
    All,
    /// Record only the items pushed to the stack
    Pushes,
    /// Record the full stack
    Full,
}

impl StackSnapshotType {
    /// Returns true if this is the [StackSnapshotType::All] variant
    #[inline]
    pub const fn is_all(self) -> bool {
        matches!(self, Self::All)
    }

    /// Returns true if this is the [StackSnapshotType::Full] variant
    #[inline]
    pub const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Returns true if this is the [StackSnapshotType::Pushes] variant
    #[inline]
    pub const fn is_pushes(self) -> bool {
        matches!(self, Self::Pushes)
    }
}

/// What kind of tracing style this is.
///
/// This affects things like error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraceStyle {
    /// Parity style tracer
    Parity,
    /// Geth style tracer
    #[allow(dead_code)]
    Geth,
}

impl TraceStyle {
    /// Returns true if this is a parity style tracer.
    pub(crate) const fn is_parity(self) -> bool {
        matches!(self, Self::Parity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parity_config() {
        let mut s = HashSet::default();
        s.insert(TraceType::StateDiff);
        let config = TracingInspectorConfig::from_parity_config(&s);
        // not required
        assert!(!config.record_steps);
        assert!(!config.record_state_diff);

        let mut s = HashSet::default();
        s.insert(TraceType::VmTrace);
        let config = TracingInspectorConfig::from_parity_config(&s);
        assert!(config.record_steps);
        assert!(!config.record_state_diff);

        let mut s = HashSet::default();
        s.insert(TraceType::VmTrace);
        s.insert(TraceType::StateDiff);
        let config = TracingInspectorConfig::from_parity_config(&s);
        assert!(config.record_steps);
        // not required for StateDiff
        assert!(!config.record_state_diff);
    }

    #[test]
    fn test_flat_call_config() {
        let config = FlatCallConfig { include_precompiles: Some(true), ..Default::default() };
        let config = TracingInspectorConfig::from_flat_call_config(&config);
        assert!(!config.exclude_precompile_calls);

        let config = FlatCallConfig { include_precompiles: Some(false), ..Default::default() };
        let config = TracingInspectorConfig::from_flat_call_config(&config);
        assert!(config.exclude_precompile_calls);
    }

    #[test]
    fn test_all_records_all_stack_snapshots() {
        // `all()` enables everything, so the stack snapshot type should be the most
        // complete variant (`All` = full stack + pushes), not `Full` (full only).
        let config = TracingInspectorConfig::all();
        assert_eq!(config.record_stack_snapshots, StackSnapshotType::All);
        assert!(config.record_stack_snapshots.is_all());
    }
}
