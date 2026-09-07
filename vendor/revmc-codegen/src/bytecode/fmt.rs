use std::{borrow::Cow, fmt, fmt::Write, io::IsTerminal};

use oxc_index::{IndexVec, index_vec};
use revm_bytecode::opcode as op;
use revm_primitives::hex;

use super::{
    Bytecode, Inst, InstData, InstFlags,
    asm::{TokenKind, Tokenizer},
    bitvec_as_bytes,
    passes::block_analysis::Block,
};

impl Bytecode<'_> {
    /// Resolves a jump target instruction to its CFG block index.
    fn target_block(&self, target: Inst) -> Option<Block> {
        self.cfg.inst_to_block.get(target).copied().flatten()
    }

    /// Collects formatted lines and builds the inst-to-line map stored in `self.inst_lines`.
    pub(super) fn collect_lines(&self) -> Vec<(String, String)> {
        let mut lines: Vec<(String, String)> = Vec::new();
        let mut inst_lines: IndexVec<Inst, u32> = index_vec![0u32; self.insts.len()];

        // Compute field widths for aligned ic=/pc= columns.
        let (max_ic, max_pc) = self
            .cfg
            .blocks
            .iter()
            .flat_map(|b| {
                b.insts().filter(|&i| !self.inst(i).is_dead_code()).map(|i| (i, self.pc(i)))
            })
            .fold((0u32, 0u32), |(mi, mp), (i, p)| (mi.max(i.index() as u32), mp.max(p)));
        let ic_width = decimal_width(max_ic);
        let pc_width = decimal_width(max_pc);

        let s = self.ir_stats();

        lines.push((
            String::new(),
            format!(
                "spec_id={} has_dynamic_jumps={} may_suspend={}",
                self.spec_id, self.has_dynamic_jumps, self.may_suspend,
            ),
        ));
        lines.push((
            String::new(),
            format!(
                "insts={} live={} dead={} noops={} suspends={} blocks={} block_min={} block_max={} block_avg={:.1} block_median={}",
                s.total, s.live, s.dead, s.noops, s.suspends, s.blocks, s.block_min, s.block_max, s.block_avg, s.block_median,
            ),
        ));
        lines.push((String::new(), String::new()));

        for (bid, block) in self.cfg.blocks.iter_enumerated() {
            // Blank line between blocks.
            if !lines.is_empty()
                && lines.last().is_some_and(|(t, c)| !t.is_empty() || !c.is_empty())
            {
                lines.push((String::new(), String::new()));
            }

            // Block header.
            let first = self.inst(block.insts.start);
            let mut header = format!("{bid}:");
            let mut comment = String::new();
            if first.is_stack_section_head() {
                write!(
                    comment,
                    "stack_in={} max_growth={}",
                    first.stack_section.inputs, first.stack_section.max_growth,
                )
                .unwrap();
            }
            if !comment.is_empty() {
                comment.push(' ');
            }
            write!(comment, "predecessors=").unwrap();
            for (i, pred) in block.preds.iter().enumerate() {
                if i > 0 {
                    comment.push(',');
                }
                write!(comment, "{pred}").unwrap();
            }
            // Pad header to align with indented instructions.
            while header.len() < 2 {
                header.push(' ');
            }
            lines.push((header, comment));

            // Instructions.
            for inst in block.insts() {
                let data = self.inst(inst);
                if data.is_dead_code() {
                    continue;
                }

                // 1-based line number (lines.len() is the 0-based index of the next line).
                inst_lines[inst] = lines.len() as u32 + 1;

                // Instruction text.
                let mut text = String::from("  ");
                let opcode = self.opcode(inst);
                write!(text, "{opcode}").unwrap();
                if data.flags.contains(InstFlags::INVALID_JUMP) {
                    text.push_str(" %invalid");
                } else if data.flags.contains(InstFlags::MULTI_JUMP) {
                    if let Some(targets) = self.multi_jump_targets(inst) {
                        for (i, &t) in targets.iter().enumerate() {
                            if i > 0 {
                                text.push(',');
                            }
                            match self.target_block(t) {
                                Some(b) => write!(text, " %{b}").unwrap(),
                                None => write!(text, " %inst{t}").unwrap(),
                            }
                        }
                    }
                } else if data.is_static_jump() {
                    let target = data.static_jump_target();
                    match self.target_block(target) {
                        Some(b) => write!(text, " %{b}").unwrap(),
                        None => write!(text, " %inst{target}").unwrap(),
                    }
                } else if data.is_jump() {
                    text.push_str(" %dynamic");
                }

                // Comment with ic, pc, and flags/behavior.
                let mut comment = String::new();
                write!(comment, "ic={:>ic_width$}", inst.index()).unwrap();
                write!(comment, " pc={:>pc_width$}", self.pc(inst)).unwrap();
                if !data.gas_section.is_empty() {
                    write!(comment, " gas={}", data.gas_section.gas_cost).unwrap();
                }
                if inst != block.insts.start && data.is_stack_section_head() {
                    write!(
                        comment,
                        " stack_in={} max_growth={}",
                        data.stack_section.inputs, data.stack_section.max_growth,
                    )
                    .unwrap();
                }
                let flags = data.flags;
                if flags.contains(InstFlags::NOOP) {
                    comment.push_str(" noop");
                }
                if flags.contains(InstFlags::DEAD_CODE) {
                    comment.push_str(" dead");
                }
                if flags.contains(InstFlags::DISABLED) {
                    comment.push_str(" disabled");
                }
                if flags.contains(InstFlags::UNKNOWN) {
                    comment.push_str(" unknown");
                }
                if flags.contains(InstFlags::INVALID_JUMP) {
                    comment.push_str(" invalid_jump");
                }
                if flags.contains(InstFlags::MULTI_JUMP) {
                    comment.push_str(" multi_jump");
                }
                if data.may_suspend() {
                    comment.push_str(" suspends");
                }
                if data.is_reachable_jumpdest(self.has_dynamic_jumps) {
                    comment.push_str(" reachable");
                }

                lines.push((text, comment));
            }
        }

        *self.inst_lines.borrow_mut() = inst_lines;
        lines
    }

    /// Formats the bytecode IR as plain text.
    fn display_plain(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines = self.collect_lines();
        let max_text_width = lines.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
        let comment_col = max_text_width.clamp(4, 20);
        for (text, comment) in &lines {
            if text.is_empty() && comment.is_empty() {
                writeln!(f)?;
            } else if comment.is_empty() {
                writeln!(f, "{text}")?;
            } else if text.is_empty() {
                writeln!(f, "; {comment}")?;
            } else {
                writeln!(f, "{text:<comment_col$} ; {comment}")?;
            }
        }
        Ok(())
    }

    /// Formats the bytecode IR with ANSI syntax highlighting.
    ///
    /// Builds the plain-text output first, then runs it through the asm
    /// tokenizer and emits colored spans.
    fn display_colored(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plain = format!("{}", std::fmt::from_fn(|f| self.display_plain(f)));
        colorize(f, &plain)
    }
}

impl fmt::Display for Bytecode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() && std::io::stdout().is_terminal() {
            self.display_colored(f)
        } else {
            self.display_plain(f)
        }
    }
}

// ———————————————————————————————————————————————————————————————————————
// Colorizer (uses asm::Tokenizer)
// ———————————————————————————————————————————————————————————————————————

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BLUE: &str = "\x1b[34m";
const WHITE: &str = "\x1b[37m";

/// Writes ANSI-colored output by tokenizing `plain` with the asm tokenizer.
fn colorize(f: &mut fmt::Formatter<'_>, plain: &str) -> fmt::Result {
    for token in Tokenizer::new(plain) {
        let s = token.src;
        match token.kind {
            TokenKind::Whitespace => write!(f, "{s}")?,
            TokenKind::Label => write!(f, "{BOLD}{CYAN}{s}:{RESET}")?,
            TokenKind::Comment => write!(f, "{DIM}{s}{RESET}")?,
            TokenKind::Number(_) => write!(f, "{YELLOW}{s}{RESET}")?,
            TokenKind::LabelRef if s == "dynamic" => write!(f, "{RED}%{s}{RESET}")?,
            TokenKind::LabelRef => write!(f, "{CYAN}%{s}{RESET}")?,
            TokenKind::Ident => {
                let color = opcode_color(s);
                write!(f, "{BOLD}{color}{s}{RESET}")?;
            }
            TokenKind::Comma => write!(f, ",")?,
            TokenKind::Unknown => write!(f, "{s}")?,
            TokenKind::LParen | TokenKind::RParen | TokenKind::ParamRef => {}
        }
    }
    Ok(())
}

/// Returns the ANSI color for an opcode name based on its byte-value category.
fn opcode_color(name: &str) -> &'static str {
    use revm_bytecode::opcode::OpCode;
    let byte = match OpCode::parse(name) {
        Some(op) => op.get(),
        // Bare "PUSH" without a number suffix.
        None if name == "PUSH" => op::PUSH32,
        None => return WHITE,
    };
    opcode_color_by_byte(byte)
}

fn opcode_color_by_byte(byte: u8) -> &'static str {
    use op::*;
    match byte {
        // Terminating.
        STOP | RETURN | REVERT | INVALID | SELFDESTRUCT => RED,
        // Side effects: jumps, calls, creates, log.
        JUMP
        | JUMPI
        | JUMPDEST
        | CREATE
        | CALL
        | CALLCODE
        | DELEGATECALL
        | CREATE2
        | STATICCALL
        | LOG0..=LOG4 => GREEN,
        // Arithmetic, comparison, bitwise, keccak.
        ADD..=SIGNEXTEND | LT..=CLZ | KECCAK256 => WHITE,
        // I/O: environment, memory, storage.
        ADDRESS..=SLOTNUM | MLOAD..=MCOPY => BLUE,
        // Stack: POP, PUSH, DUP, SWAP, EOF stack ops.
        POP | PUSH0..=SWAP16 | DUPN | SWAPN | EXCHANGE => YELLOW,
        _ => WHITE,
    }
}

impl fmt::Debug for Bytecode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bytecode")
            .field("code", &hex::encode(&*self.code))
            .field("insts", &self.insts)
            .field("jumpdests", &hex::encode(bitvec_as_bytes(&self.jumpdests)))
            .field("spec_id", &self.spec_id)
            .field("has_dynamic_jumps", &self.has_dynamic_jumps)
            .field("may_suspend", &self.may_suspend)
            .finish()
    }
}

impl fmt::Debug for InstData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstData")
            .field("opcode", &self.to_op())
            .field("flags", &format_args!("{:?}", self.flags))
            .field("data", &self.data)
            .field("gas_section", &self.gas_section)
            .field("stack_section", &self.stack_section)
            .finish()
    }
}

// DOT graph colors.
mod dot_colors {
    const DARK_NAVY: &str = "#1a1a2e";
    const DARK_BLUE: &str = "#16213e";
    const BLUE: &str = "#0f3460";
    const DARK_TEAL: &str = "#1a2340";
    const TEAL: &str = "#53a8b6";
    const GREEN: &str = "#5cdb95";
    const DARK_GREEN: &str = "#1a2e1a";
    const DARK_ORANGE: &str = "#2e2416";
    const ORANGE: &str = "#e0a030";
    const DARK_RED: &str = "#2d1b2e";
    const RED: &str = "#e94560";
    const GRAY: &str = "#555577";
    const LIGHT_GRAY: &str = "#e0e0e0";

    pub(super) const BG: &str = DARK_NAVY;
    pub(super) const TEXT: &str = LIGHT_GRAY;
    // Default node.
    pub(super) const NODE_FILL: &str = DARK_BLUE;
    pub(super) const NODE_BORDER: &str = BLUE;
    // Reverting/error blocks.
    pub(super) const REVERT_FILL: &str = DARK_RED;
    pub(super) const REVERT_BORDER: &str = RED;
    // Non-reverting exit blocks (STOP, RETURN).
    pub(super) const EXIT_FILL: &str = DARK_GREEN;
    pub(super) const EXIT_BORDER: &str = GREEN;
    // Suspending blocks (CALL, CREATE, ...).
    pub(super) const SUSPEND_FILL: &str = DARK_ORANGE;
    pub(super) const SUSPEND_BORDER: &str = ORANGE;
    // Branching blocks.
    pub(super) const BRANCH_FILL: &str = DARK_TEAL;
    pub(super) const BRANCH_BORDER: &str = TEAL;
    // Edges.
    pub(super) const EDGE: &str = GRAY;
    pub(super) const EDGE_JUMP: &str = TEAL;
    pub(super) const EDGE_COND_JUMP: &str = GREEN;
    pub(super) const EDGE_FALSE: &str = RED;
}

impl<'a> Bytecode<'a> {
    /// Writes the bytecode as a DOT graph to the given writer.
    #[doc(hidden)]
    pub fn write_dot<W: fmt::Write>(&self, w: &mut W) -> fmt::Result {
        use dot_colors::*;

        writeln!(w, "digraph bytecode {{")?;
        writeln!(w, "  graph [bgcolor=\"{BG}\" rankdir=TB];")?;
        writeln!(
            w,
            "  node [shape=box style=\"rounded,filled\" fontname=\"Courier\" fontsize=10 \
             fillcolor=\"{NODE_FILL}\" fontcolor=\"{TEXT}\" \
             color=\"{NODE_BORDER}\" penwidth=1.5];"
        )?;
        writeln!(
            w,
            "  edge [fontname=\"Courier\" fontsize=9 color=\"{EDGE}\" tailport=s headport=n];"
        )?;

        // Emit nodes.
        for (bid, block) in self.cfg.blocks.iter_enumerated() {
            let last = self.inst(block.terminator());
            let first = self.inst(block.insts.start);

            // Color based on block behavior.
            let has_suspend = block.insts().any(|i| self.inst(i).may_suspend());
            let (fill, border) = if matches!(last.opcode, op::STOP | op::RETURN) {
                (EXIT_FILL, EXIT_BORDER)
            } else if last.is_diverging() {
                (REVERT_FILL, REVERT_BORDER)
            } else if has_suspend {
                (SUSPEND_FILL, SUSPEND_BORDER)
            } else if last.is_jump() {
                (BRANCH_FILL, BRANCH_BORDER)
            } else {
                (NODE_FILL, NODE_BORDER)
            };

            write!(
                w,
                "  {bid} [fillcolor=\"{fill}\" color=\"{border}\" \
                 label=\"{bid}",
            )?;

            if first.is_stack_section_head() {
                write!(
                    w,
                    " [in={} growth={}]",
                    first.stack_section.inputs, first.stack_section.max_growth
                )?;
            }

            write!(w, "\\n")?;
            for inst in block.insts() {
                let data = self.inst(inst);
                if data.is_dead_code() {
                    continue;
                }
                // Show stack section header for mid-block section boundaries.
                if inst != block.insts.start && data.is_stack_section_head() {
                    write!(
                        w,
                        "--- [in={} growth={}]\\l",
                        data.stack_section.inputs, data.stack_section.max_growth
                    )?;
                }
                let opcode = self.opcode(inst);
                let mut op_str =
                    abbreviate_hex(&opcode.to_string()).replace('>', "\\>").replace('<', "\\<");
                if !data.gas_section.is_empty() {
                    write!(op_str, " [g={}]", data.gas_section.gas_cost).unwrap();
                }
                write!(w, "{op_str}\\l")?;
            }
            writeln!(w, "\"];")?;
        }

        // Emit edges from the CFG.
        for (bid, block) in self.cfg.blocks.iter_enumerated() {
            let last = self.inst(block.terminator());

            if last.is_jump()
                && !last.is_static_jump()
                && !last.flags.contains(InstFlags::MULTI_JUMP)
            {
                writeln!(w, "  {bid} -> dynamic [color=\"{EDGE_FALSE}\" style=dashed];")?;
                continue;
            }

            // Succs layout: [fallthrough?, jump_target(s)...].
            // Fallthrough is first when present (JUMPI or non-branching terminator).
            let mut succs = block.succs.iter().copied();
            if last.can_fall_through()
                && let Some(ft) = succs.next()
            {
                let color = if last.opcode == op::JUMPI { EDGE_FALSE } else { EDGE };
                writeln!(w, "  {bid} -> {ft} [color=\"{color}\"];")?;
            }
            // Remaining succs are jump targets.
            let is_multi = last.flags.contains(InstFlags::MULTI_JUMP) && block.succs.len() > 1;
            for target in succs {
                let color = if is_multi {
                    "#e2a93b"
                } else if last.opcode == op::JUMPI {
                    EDGE_COND_JUMP
                } else {
                    EDGE_JUMP
                };
                let extra = if target <= bid { " constraint=false" } else { "" };
                writeln!(w, "  {bid} -> {target} [color=\"{color}\"{extra}];")?;
            }
        }

        // Dynamic jump table.
        if self.has_dynamic_jumps {
            writeln!(
                w,
                "  dynamic [shape=diamond style=filled fillcolor=\"{REVERT_FILL}\" \
                 color=\"{REVERT_BORDER}\" fontcolor=\"{TEXT}\" \
                 label=\"dynamic\\njump table\"];"
            )?;
            for (bid, block) in self.cfg.blocks.iter_enumerated() {
                let first = self.inst(block.insts.start);
                if first.is_reachable_jumpdest(self.has_dynamic_jumps) {
                    writeln!(w, "  dynamic -> {bid} [color=\"{EDGE_FALSE}\" style=dashed];")?;
                }
            }
        }

        writeln!(w, "}}")
    }

    /// Returns the bytecode as a DOT graph string.
    #[cfg(test)]
    fn to_dot(&self) -> String {
        let mut s = String::new();
        self.write_dot(&mut s).unwrap();
        s
    }
}

fn decimal_width(n: u32) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

/// Abbreviates hex strings with repeated leading byte pairs.
/// E.g. `"PUSH32 0xffffffffff...ffe0"` → `"PUSH32 0xff..ffe0"`.
fn abbreviate_hex(s: &str) -> Cow<'_, str> {
    let Some(hex_start) = s.find("0x") else {
        return Cow::Borrowed(s);
    };
    let hex = &s[hex_start + 2..];
    // Need at least 2 byte pairs (4 hex chars) of repetition to abbreviate.
    if hex.len() < 8 {
        return Cow::Borrowed(s);
    }
    let prefix = &hex[..2];
    let run_len = hex
        .as_bytes()
        .chunks(2)
        .take_while(|chunk| chunk.len() == 2 && *chunk == prefix.as_bytes())
        .count();
    if run_len < 4 {
        return Cow::Borrowed(s);
    }
    let suffix = &hex[run_len * 2..];
    Cow::Owned(format!("{}0x{prefix}..{suffix}", &s[..hex_start]))
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;
    use revm_primitives::hardfork::SpecId;

    use super::*;

    /// Test bytecode with SSTORE (splits gas but not stack), a loop (back-edge), and CALL
    /// (suspending instruction that splits both gas and stack sections).
    fn test_bytecode() -> Bytecode<'static> {
        #[rustfmt::skip]
        let code: &[u8] = &[
            op::PUSH1, 0x03,
            op::JUMP,
            op::JUMPDEST,
            op::PUSH1, 0x01,
            op::PUSH1, 0x00,
            op::SSTORE,
            op::CALLDATASIZE,
            op::PUSH1, 0x03,
            op::JUMPI,
            op::PUSH1, 0x00,
            op::PUSH1, 0x00,
            op::PUSH1, 0x00,
            op::PUSH1, 0x00,
            op::PUSH1, 0x00,
            op::PUSH1, 0x42,
            op::PUSH2, 0xff, 0xff,
            op::CALL,
            op::POP,
            op::STOP,
        ];
        let mut bytecode = Bytecode::new(code, SpecId::OSAKA, None);
        bytecode.analyze().unwrap();
        bytecode
    }

    #[test]
    fn display_format() {
        let bytecode = test_bytecode();
        let actual = format!("{bytecode}");
        snapbox_1_1::assert_data_eq!(
            actual,
            snapbox_1_1::str![[r#"
; spec_id=Osaka has_dynamic_jumps=false may_suspend=true
; insts=19 live=19 dead=0 noops=10 suspends=1 blocks=3 block_min=2 block_max=10 block_avg=6.3 block_median=7

bb0:           ; stack_in=0 max_growth=1 predecessors=
  PUSH1 0x03   ; ic= 0 pc= 0 gas=11 noop
  JUMP %bb1    ; ic= 1 pc= 2

bb1:           ; stack_in=0 max_growth=2 predecessors=bb0,bb1
  JUMPDEST     ; ic= 2 pc= 3 gas=7 reachable
  PUSH1 0x01   ; ic= 3 pc= 4 noop
  PUSH1 0x00   ; ic= 4 pc= 6 noop
  SSTORE       ; ic= 5 pc= 8
  CALLDATASIZE ; ic= 6 pc= 9 gas=15 stack_in=0 max_growth=2
  PUSH1 0x03   ; ic= 7 pc=10 noop
  JUMPI %bb1   ; ic= 8 pc=12

bb2:           ; stack_in=0 max_growth=7 predecessors=bb1
  PUSH1 0x00   ; ic= 9 pc=13 gas=121
  PUSH1 0x00   ; ic=10 pc=15 noop
  PUSH1 0x00   ; ic=11 pc=17 noop
  PUSH1 0x00   ; ic=12 pc=19 noop
  PUSH1 0x00   ; ic=13 pc=21 noop
  PUSH1 0x42   ; ic=14 pc=23 noop
  PUSH2 0xffff ; ic=15 pc=25 noop
  CALL         ; ic=16 pc=28 suspends
  POP          ; ic=17 pc=29 gas=2 stack_in=1 max_growth=0
  STOP         ; ic=18 pc=30

"#]]
        );
    }

    #[test]
    fn display_roundtrip() {
        let bytecode = test_bytecode();
        let displayed = format!("{bytecode}");

        // Strip the resolved jump-target refs (e.g. ` %bb1`) from JUMP/JUMPI
        // lines — these are display metadata, not asm operands.
        let stripped: String = displayed
            .lines()
            .map(|line| {
                let (before_comment, comment) = line.split_once(';').unwrap_or((line, ""));
                let cleaned = before_comment
                    .replace(" %bb", " ; %bb")
                    .replace(" %dynamic", " ; %dynamic")
                    .replace(" %invalid", " ; %invalid");
                if comment.is_empty() {
                    format!("{cleaned}\n")
                } else {
                    format!("{cleaned};{comment}\n")
                }
            })
            .collect();

        println!("{stripped}");
        let reparsed = crate::parse_asm(&stripped).unwrap();
        assert_eq!(reparsed, bytecode.code.as_ref(), "display output did not roundtrip");
    }

    #[test]
    fn dot_format() {
        let bytecode = test_bytecode();
        let dot = bytecode.to_dot();
        eprintln!("{dot}");
        assert!(dot.starts_with("digraph bytecode {"));
        assert!(dot.contains("bb0"));
        assert!(dot.contains("bb1"));
        assert!(dot.contains("bb2"));
        // SSTORE present in bb1.
        assert!(dot.contains("SSTORE"), "missing SSTORE");
        // SSTORE splits gas sections: two [g=] annotations in bb1.
        assert!(dot.contains("[g=7]"), "missing first gas section");
        assert!(dot.contains("[g=15]"), "missing second gas section");
        // CALL present in bb2.
        assert!(dot.contains("CALL"), "missing CALL");
        assert!(dot.contains("[g=121]"), "missing CALL gas section");
        // bb0 -> bb1 (unconditional jump).
        assert!(dot.contains("bb0 -> bb1"), "missing jump edge");
        // bb1 -> bb1 (loop back-edge).
        assert!(dot.contains("bb1 -> bb1"), "missing loop back-edge");
        // bb1 -> bb2 (fallthrough on false).
        assert!(dot.contains("bb1 -> bb2"), "missing false edge");
        assert!(!dot.contains("dynamic"), "unexpected dynamic jump table");
    }

    #[test]
    fn abbreviate_hex_repeated() {
        // 32 repeated ff bytes + suffix.
        assert_eq!(
            abbreviate_hex(
                "PUSH32 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0"
            ),
            "PUSH32 0xff..e0",
        );
        assert_eq!(
            abbreviate_hex(
                "PUSH32 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeeee"
            ),
            "PUSH32 0xff..eeee",
        );
        assert_eq!(
            abbreviate_hex(
                "PUSH32 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeeeeee"
            ),
            "PUSH32 0xff..eeeeee",
        );
        // 32 repeated 00 bytes + suffix.
        assert_eq!(
            abbreviate_hex(
                "PUSH32 0x0000000000000000000000000000000000000000000000000000000000000001"
            ),
            "PUSH32 0x00..01",
        );
        // All repeated, no suffix.
        assert_eq!(
            abbreviate_hex(
                "PUSH32 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            ),
            "PUSH32 0xff..",
        );
    }

    #[test]
    fn abbreviate_hex_short() {
        // Too few repeated pairs (< 4).
        assert_eq!(abbreviate_hex("PUSH3 0xffffff"), "PUSH3 0xffffff");
        // No repetition.
        assert_eq!(abbreviate_hex("PUSH4 0x30627b7c"), "PUSH4 0x30627b7c");
        // Short value.
        assert_eq!(abbreviate_hex("PUSH1 0x40"), "PUSH1 0x40");
        // No hex at all.
        assert_eq!(abbreviate_hex("STOP"), "STOP");
    }
}
