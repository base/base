use super::{
    types::{
        CallKind, CallLog, CallTrace, CallTraceNode, DecodedCallData, DecodedTraceStep,
        TraceMemberOrder,
    },
    CallTraceArena,
};
use alloc::{format, string::String, vec::Vec};
use alloy_primitives::{address, hex, map::HashMap, Address, B256, U256};
use anstyle::{AnsiColor, Color, Style};
use colorchoice::ColorChoice;
use revm::interpreter::InstructionResult;
use std::io::{self, Write};

const CHEATCODE_ADDRESS: Address = address!("7109709ECfa91a80626fF3989D68f67F5b1DD12D");

const PIPE: &str = "  │ ";
const EDGE: &str = "  └─ ";
const BRANCH: &str = "  ├─ ";
const CALL: &str = "→ ";
const RETURN: &str = "← ";

const TRACE_KIND_STYLE: Style = AnsiColor::Yellow.on_default();
const LOG_STYLE: Style = AnsiColor::Cyan.on_default();

/// Configuration for a [`TraceWriter`].
#[derive(Clone, Debug)]
#[allow(missing_copy_implementations)]
pub struct TraceWriterConfig {
    use_colors: bool,
    color_cheatcodes: bool,
    write_bytecodes: bool,
    write_storage_changes: bool,
}

impl Default for TraceWriterConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceWriterConfig {
    /// Create a new `TraceWriterConfig` with default settings.
    pub fn new() -> Self {
        Self {
            use_colors: use_colors(ColorChoice::Auto),
            color_cheatcodes: false,
            write_bytecodes: false,
            write_storage_changes: false,
        }
    }

    /// Use colors in the output. Default: [`Auto`](ColorChoice::Auto).
    pub fn color_choice(mut self, choice: ColorChoice) -> Self {
        self.use_colors = use_colors(choice);
        self
    }

    /// Get the current color choice. `Auto` is lost, so this returns `true` if colors are enabled.
    pub fn get_use_colors(&self) -> bool {
        self.use_colors
    }

    /// Color calls to the cheatcode address differently. Default: false.
    pub fn color_cheatcodes(mut self, yes: bool) -> Self {
        self.color_cheatcodes = yes;
        self
    }

    /// Returns `true` if calls to the cheatcode address are colored differently.
    pub fn get_color_cheatcodes(&self) -> bool {
        self.color_cheatcodes
    }

    /// Write contract creation codes and deployed codes when writing "create" traces.
    /// Default: false.
    pub fn write_bytecodes(mut self, yes: bool) -> Self {
        self.write_bytecodes = yes;
        self
    }

    /// Returns `true` if contract creation codes and deployed codes are written.
    pub fn get_write_bytecodes(&self) -> bool {
        self.write_bytecodes
    }

    /// Sets whether to write storage changes.
    pub fn write_storage_changes(mut self, yes: bool) -> Self {
        self.write_storage_changes = yes;
        self
    }

    /// Returns `true` if storage changes are written to the writer.
    pub fn get_write_storage_changes(&self) -> bool {
        self.write_storage_changes
    }
}

/// Formats [call traces](CallTraceArena) to an [`Write`] writer.
///
/// Will never write invalid UTF-8.
#[derive(Clone, Debug)]
pub struct TraceWriter<W> {
    writer: W,
    indentation_level: u16,
    config: TraceWriterConfig,
}

impl<W: Write> TraceWriter<W> {
    /// Create a new `TraceWriter` with the given writer.
    #[inline]
    pub fn new(writer: W) -> Self {
        Self::with_config(writer, TraceWriterConfig::new())
    }

    /// Create a new `TraceWriter` with the given writer and configuration.
    pub fn with_config(writer: W, config: TraceWriterConfig) -> Self {
        Self { writer, indentation_level: 0, config }
    }

    /// Sets the color choice.
    #[inline]
    pub fn use_colors(mut self, color_choice: ColorChoice) -> Self {
        self.config.use_colors = use_colors(color_choice);
        self
    }

    /// Sets whether to color calls to the cheatcode address differently.
    #[inline]
    pub fn color_cheatcodes(mut self, yes: bool) -> Self {
        self.config.color_cheatcodes = yes;
        self
    }

    /// Sets the starting indentation level.
    #[inline]
    pub fn with_indentation_level(mut self, level: u16) -> Self {
        self.indentation_level = level;
        self
    }

    /// Sets whether contract creation codes and deployed codes should be written.
    #[inline]
    pub fn write_bytecodes(mut self, yes: bool) -> Self {
        self.config.write_bytecodes = yes;
        self
    }

    /// Sets whether to write storage changes.
    #[inline]
    pub fn with_storage_changes(mut self, yes: bool) -> Self {
        self.config.write_storage_changes = yes;
        self
    }

    /// Returns a reference to the inner writer.
    #[inline]
    pub const fn writer(&self) -> &W {
        &self.writer
    }

    /// Returns a mutable reference to the inner writer.
    #[inline]
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consumes the `TraceWriter` and returns the inner writer.
    #[inline]
    pub fn into_writer(self) -> W {
        self.writer
    }

    /// Writes a call trace arena to the writer.
    pub fn write_arena(&mut self, arena: &CallTraceArena) -> io::Result<()> {
        self.write_node(arena.nodes(), 0)?;
        self.writer.flush()
    }

    /// Writes a single item of a single node to the writer. Returns the index of the next item to
    /// be written.
    ///
    /// Note: this will return length of [CallTraceNode::ordering] when last item will get
    /// processed.
    fn write_item(
        &mut self,
        nodes: &[CallTraceNode],
        node_idx: usize,
        item_idx: usize,
    ) -> io::Result<usize> {
        let node = &nodes[node_idx];
        match &node.ordering[item_idx] {
            TraceMemberOrder::Log(index) => {
                self.write_log(&node.logs[*index])?;
                Ok(item_idx + 1)
            }
            TraceMemberOrder::Call(index) => {
                self.write_node(nodes, node.children[*index])?;
                Ok(item_idx + 1)
            }
            TraceMemberOrder::Step(index) => self.write_step(nodes, node_idx, item_idx, *index),
        }
    }

    /// Writes items of a single node to the writer, starting from the given index, and until the
    /// given predicate is false.
    ///
    /// Returns the index of the next item to be written.
    fn write_items_until(
        &mut self,
        nodes: &[CallTraceNode],
        node_idx: usize,
        first_item_idx: usize,
        f: impl Fn(usize) -> bool,
    ) -> io::Result<usize> {
        let mut item_idx = first_item_idx;
        while !f(item_idx) {
            item_idx = self.write_item(nodes, node_idx, item_idx)?;
        }
        Ok(item_idx)
    }

    /// Writes all items of a single node to the writer.
    fn write_items(&mut self, nodes: &[CallTraceNode], node_idx: usize) -> io::Result<()> {
        let items_cnt = nodes[node_idx].ordering.len();
        self.write_items_until(nodes, node_idx, 0, |idx| idx == items_cnt)?;
        Ok(())
    }

    /// Writes a single node and its children to the writer.
    fn write_node(&mut self, nodes: &[CallTraceNode], idx: usize) -> io::Result<()> {
        let node = &nodes[idx];

        // Write header.
        self.write_branch()?;
        self.write_trace_header(&node.trace)?;
        self.writer.write_all(b"\n")?;

        // Write logs and subcalls.
        self.indentation_level += 1;
        self.write_items(nodes, idx)?;

        if self.config.write_storage_changes {
            self.write_storage_changes(node)?;
        }

        // Write return data.
        self.write_edge()?;
        self.write_trace_footer(&node.trace)?;
        self.writer.write_all(b"\n")?;

        self.indentation_level -= 1;

        Ok(())
    }

    /// Writes the header of a call trace.
    fn write_trace_header(&mut self, trace: &CallTrace) -> io::Result<()> {
        write!(self.writer, "[{}] ", trace.gas_used)?;

        let trace_kind_style = self.trace_kind_style();
        let address = trace.address.to_checksum_buffer(None);

        if trace.kind.is_any_create() {
            write!(
                self.writer,
                "{trace_kind_style}{CALL}new{trace_kind_style:#} {label}@{address}",
                label = trace.decoded_label("<unknown>")
            )?;
            if self.config.write_bytecodes {
                write!(self.writer, "({})", trace.data)?;
            }
        } else {
            let (func_name, inputs) = match trace.decoded_call_data() {
                Some(DecodedCallData { signature, args }) => {
                    let name = signature.split('(').next().unwrap();
                    (name.to_string(), args.join(", "))
                }
                None => {
                    if trace.data.len() < 4 && !trace.value.is_zero() {
                        // Short calldata + nonzero value = receive() was invoked
                        ("receive".to_string(), String::new())
                    } else if trace.data.len() < 4 {
                        ("fallback".to_string(), hex::encode(&trace.data))
                    } else {
                        let (selector, data) = trace.data.split_at(4);
                        (hex::encode(selector), hex::encode(data))
                    }
                }
            };

            write!(
                self.writer,
                "{style}{addr}{style:#}::{style}{func_name}{style:#}",
                style = self.trace_style(trace),
                addr = trace.decoded_label(address.as_str()),
            )?;

            if !trace.value.is_zero() {
                write!(self.writer, "{{value: {}}}", trace.value)?;
            }

            write!(self.writer, "({inputs})")?;

            let action = match trace.kind {
                CallKind::Call => None,
                CallKind::StaticCall => Some(" [staticcall]"),
                CallKind::CallCode => Some(" [callcode]"),
                CallKind::DelegateCall => Some(" [delegatecall]"),
                CallKind::AuthCall => Some(" [authcall]"),
                CallKind::Create | CallKind::Create2 => unreachable!(),
            };
            if let Some(action) = action {
                write!(self.writer, "{trace_kind_style}{action}{trace_kind_style:#}")?;
            }
        }

        Ok(())
    }

    fn write_log(&mut self, log: &CallLog) -> io::Result<()> {
        let log_style = self.log_style();
        self.write_branch()?;

        if let Some(name) = log.decoded_name() {
            write!(self.writer, "emit {name}({log_style}")?;
            if let Some(params) = log.decoded_params() {
                for (i, (param_name, value)) in params.iter().enumerate() {
                    if i > 0 {
                        self.writer.write_all(b", ")?;
                    }
                    write!(self.writer, "{param_name}: {value}")?;
                }
            }
            writeln!(self.writer, "{log_style:#})")?;
        } else {
            for (i, topic) in log.raw_log.topics().iter().enumerate() {
                if i == 0 {
                    self.writer.write_all(b" emit topic 0")?;
                } else {
                    self.write_pipes()?;
                    write!(self.writer, "       topic {i}")?;
                }
                writeln!(self.writer, ": {log_style}{topic}{log_style:#}")?;
            }

            if !log.raw_log.topics().is_empty() {
                self.write_pipes()?;
            }
            writeln!(
                self.writer,
                "          data: {log_style}{data}{log_style:#}",
                data = log.raw_log.data
            )?;
        }

        Ok(())
    }

    /// Writes a single step of a single node to the writer. Returns the index of the next item to
    /// be written.
    fn write_step(
        &mut self,
        nodes: &[CallTraceNode],
        node_idx: usize,
        item_idx: usize,
        step_idx: usize,
    ) -> io::Result<usize> {
        let node = &nodes[node_idx];
        let step = &node.trace.steps[step_idx];

        let Some(decoded) = &step.decoded else {
            // We only write explicitly decoded steps to avoid bloating the output.
            return Ok(item_idx + 1);
        };

        match &**decoded {
            DecodedTraceStep::InternalCall(call, end_idx) => {
                let gas_used = node.trace.steps[*end_idx].gas_used.saturating_sub(step.gas_used);

                self.write_branch()?;
                self.indentation_level += 1;

                writeln!(
                    self.writer,
                    "[{}] {}{}",
                    gas_used,
                    call.func_name,
                    call.args.as_ref().map(|v| format!("({})", v.join(", "))).unwrap_or_default()
                )?;

                let end_item_idx =
                    self.write_items_until(nodes, node_idx, item_idx + 1, |item_idx: usize| {
                        matches!(&node.ordering[item_idx], TraceMemberOrder::Step(idx) if *idx == *end_idx)
                    })?;

                self.write_edge()?;
                write!(self.writer, "{RETURN}")?;

                if let Some(outputs) = &call.return_data {
                    write!(self.writer, "{}", outputs.join(", "))?;
                }

                writeln!(self.writer)?;

                self.indentation_level -= 1;

                Ok(end_item_idx + 1)
            }
            DecodedTraceStep::Line(line) => {
                self.write_branch()?;
                writeln!(self.writer, "{line}")?;

                Ok(item_idx + 1)
            }
        }
    }

    /// Writes the footer of a call trace.
    fn write_trace_footer(&mut self, trace: &CallTrace) -> io::Result<()> {
        write!(
            self.writer,
            "{style}{RETURN}[{status:?}]{style:#}",
            style = self.trace_style(trace),
            status = trace.status.unwrap_or(InstructionResult::Stop),
        )?;

        if let Some(decoded) = trace.decoded_return_data() {
            write!(self.writer, " ")?;
            return self.writer.write_all(decoded.as_bytes());
        }

        if !self.config.write_bytecodes
            && (trace.kind.is_any_create() && trace.status.is_none_or(|status| status.is_ok()))
        {
            write!(self.writer, " {} bytes of code", trace.output.len())?;
        } else if !trace.output.is_empty() {
            write!(self.writer, " {}", trace.output)?;
        }

        Ok(())
    }

    fn write_indentation(&mut self) -> io::Result<()> {
        self.writer.write_all(b"  ")?;
        for _ in 1..self.indentation_level {
            self.writer.write_all(PIPE.as_bytes())?;
        }
        Ok(())
    }

    #[doc(alias = "left_prefix")]
    fn write_branch(&mut self) -> io::Result<()> {
        self.write_indentation()?;
        if self.indentation_level != 0 {
            self.writer.write_all(BRANCH.as_bytes())?;
        }
        Ok(())
    }

    #[doc(alias = "right_prefix")]
    fn write_pipes(&mut self) -> io::Result<()> {
        self.write_indentation()?;
        self.writer.write_all(PIPE.as_bytes())
    }

    fn write_edge(&mut self) -> io::Result<()> {
        self.write_indentation()?;
        self.writer.write_all(EDGE.as_bytes())
    }

    fn trace_style(&self, trace: &CallTrace) -> Style {
        if !self.config.use_colors {
            return Style::default();
        }
        let color = if self.config.color_cheatcodes && trace.address == CHEATCODE_ADDRESS {
            AnsiColor::Blue
        } else if trace.success {
            AnsiColor::Green
        } else {
            AnsiColor::Red
        };
        Color::Ansi(color).on_default()
    }

    fn trace_kind_style(&self) -> Style {
        if !self.config.use_colors {
            return Style::default();
        }
        TRACE_KIND_STYLE
    }

    fn log_style(&self) -> Style {
        if !self.config.use_colors {
            return Style::default();
        }
        LOG_STYLE
    }

    fn write_storage_changes(&mut self, node: &CallTraceNode) -> io::Result<()> {
        let mut changes_map = HashMap::new();

        // For each call trace, compact the results so we do not write the intermediate storage
        // writes. Store the change id so we can write the storage changes in chronological order
        // of their last write.
        let mut change_id = 0;
        for step in &node.trace.steps {
            if let Some(change) = &step.storage_change {
                // Only handle storage changes, signalled by `had_value` being `Some`.
                if change.had_value.is_some() {
                    let (_first, last, last_change_id) =
                        changes_map.entry(&change.key).or_insert((change, change, change_id));
                    *last = change;
                    *last_change_id = change_id;
                    change_id += 1;
                }
            }
        }

        let mut changes = changes_map.into_iter().collect::<Vec<_>>();
        changes.sort_by_key(|(_, a)| a.2);
        let changes = changes
            .iter()
            .filter_map(|(&key, (first, last, _))| {
                let value_before = first.had_value.unwrap_or_default();
                let value_after = last.value;
                if value_before == value_after {
                    return None;
                }
                Some((key, value_before, value_after))
            })
            .collect::<Vec<_>>();

        if !changes.is_empty() {
            self.write_branch()?;
            writeln!(self.writer, " storage changes:")?;
            for (key, value_before, value_after) in changes {
                self.write_pipes()?;
                writeln!(
                    self.writer,
                    "  @ {key}: {value_before} → {value_after}",
                    key = num_or_hex(key),
                    value_before = num_or_hex(value_before),
                    value_after = num_or_hex(value_after),
                )?;
            }
        }

        Ok(())
    }
}

fn use_colors(choice: ColorChoice) -> bool {
    use io::IsTerminal;
    match choice {
        ColorChoice::Auto => io::stdout().is_terminal(),
        ColorChoice::AlwaysAnsi | ColorChoice::Always => true,
        ColorChoice::Never => false,
    }
}

/// Formats the given U256 as a decimal number if it is short, otherwise as a hexadecimal
/// byte-array.
fn num_or_hex(x: U256) -> String {
    if x < U256::from(1e6 as u128) {
        x.to_string()
    } else {
        B256::from(x).to_string()
    }
}
