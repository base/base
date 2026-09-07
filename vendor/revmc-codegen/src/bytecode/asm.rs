//! EVM bytecode assembler.

use std::cmp::Ordering;

use revm_bytecode::opcode::{self as op, OpCode};
use revm_primitives::map::HashMap;

use crate::{
    U256, encode_pair, encode_single,
    eyre::{self, Result},
};

/// Parse EVM assembly from a string into bytecode.
///
/// Assembles EVM mnemonics from a string into raw bytecode. Supports:
/// - Standard EVM opcodes (`ADD`, `PUSH1 0x42`, etc.)
/// - Auto-sized pushes (`PUSH 0x1234` picks the smallest encoding)
/// - Labels: `name:` defines a label at the current PC, `PUSH %name` / `PUSHn %name` resolves to
///   the label's byte offset
/// - Comments starting with `;`
/// - C-style `#define` macros (textual expansion before parsing), with optional parameters
///
/// ```evm
/// #define PUSH_TWO(a, b) PUSH $a PUSH $b
///
/// entry:
///   PUSH_TWO(1, 2)
///   ADD
///   PUSH %target
///   JUMP
///
/// target:
///   JUMPDEST
///   STOP
/// ```
pub fn parse_asm(s: &str) -> Result<Vec<u8>> {
    let tokens = preprocess(s)?;
    let items = parse_items(&tokens)?;
    layout_and_emit(&items)
}

// ———————————————————————————————————————————————————————————————————————
// Tokenizer
// ———————————————————————————————————————————————————————————————————————

/// A token produced by the tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Token<'a> {
    /// The source text slice this token was produced from.
    pub src: &'a str,
    /// The kind of token.
    pub kind: TokenKind,
}

/// The kind of a [`Token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TokenKind {
    /// An identifier (opcode name, macro name, etc.).
    Ident,
    /// A label definition (`name:`).
    Label,
    /// A label reference (`%name`).
    LabelRef,
    /// A numeric literal.
    Number(U256),
    /// A comma.
    Comma,
    /// Opening parenthesis.
    LParen,
    /// Closing parenthesis.
    RParen,
    /// A macro parameter reference (`$name`).
    ParamRef,
    /// Whitespace.
    Whitespace,
    /// Comment text including the leading `;`.
    Comment,
    /// Unrecognized input.
    Unknown,
}

/// Character-by-character tokenizer over source text.
///
/// Always emits all tokens including whitespace and comments.
pub(super) struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    pub(super) fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    /// Read a contiguous word of alphanumeric/underscore characters.
    fn read_word(&mut self) -> &'a str {
        let rest = self.remaining();
        let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(rest.len());
        let word = &rest[..end];
        self.advance(end);
        word
    }

    /// Read a numeric literal (decimal or 0x hex).
    fn read_number(&mut self) -> Token<'a> {
        let rest = self.remaining();
        let end = if rest.starts_with("0x") || rest.starts_with("0X") {
            2 + rest[2..].find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(rest.len() - 2)
        } else {
            rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())
        };
        let src = &rest[..end];
        self.advance(end);
        match src.parse::<U256>() {
            Ok(n) => Token { src, kind: TokenKind::Number(n) },
            Err(_) => Token { src, kind: TokenKind::Unknown },
        }
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        // Emit whitespace as a token.
        let rest = self.remaining();
        let trimmed = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let ws_len = rest.len() - trimmed.len();
        if ws_len > 0 {
            let src = &self.src[self.pos..self.pos + ws_len];
            self.advance(ws_len);
            return Some(Token { src, kind: TokenKind::Whitespace });
        }

        let c = self.peek_char()?;

        match c {
            ';' => {
                let start = self.pos;
                if let Some(nl) = self.remaining().find('\n') {
                    self.advance(nl);
                } else {
                    self.pos = self.src.len();
                }
                Some(Token { src: &self.src[start..self.pos], kind: TokenKind::Comment })
            }

            '%' => {
                let start = self.pos;
                self.advance(1);
                let name = self.read_word();
                Some(if name.is_empty() {
                    Token { src: &self.src[start..self.pos], kind: TokenKind::Unknown }
                } else {
                    Token { src: name, kind: TokenKind::LabelRef }
                })
            }

            '$' => {
                let start = self.pos;
                self.advance(1);
                let name = self.read_word();
                Some(if name.is_empty() {
                    Token { src: &self.src[start..self.pos], kind: TokenKind::Unknown }
                } else {
                    Token { src: name, kind: TokenKind::ParamRef }
                })
            }

            ',' => {
                let src = &self.src[self.pos..self.pos + 1];
                self.advance(1);
                Some(Token { src, kind: TokenKind::Comma })
            }
            '(' => {
                let src = &self.src[self.pos..self.pos + 1];
                self.advance(1);
                Some(Token { src, kind: TokenKind::LParen })
            }
            ')' => {
                let src = &self.src[self.pos..self.pos + 1];
                self.advance(1);
                Some(Token { src, kind: TokenKind::RParen })
            }

            '0'..='9' => Some(self.read_number()),

            _ if c.is_ascii_alphabetic() || c == '_' => {
                let word = self.read_word();
                if self.peek_char() == Some(':') {
                    self.advance(1);
                    Some(Token { src: word, kind: TokenKind::Label })
                } else {
                    Some(Token { src: word, kind: TokenKind::Ident })
                }
            }

            ':' => {
                self.advance(1);
                Some(Token { src: "", kind: TokenKind::Label })
            }

            _ => {
                let start = self.pos;
                // Consume consecutive unrecognized characters.
                while let Some(c) = self.peek_char() {
                    if c.is_ascii_whitespace()
                        || c.is_ascii_alphanumeric()
                        || matches!(c, '_' | ';' | '%' | '$' | ',' | '(' | ')' | ':')
                    {
                        break;
                    }
                    self.advance(c.len_utf8());
                }
                Some(Token { src: &self.src[start..self.pos], kind: TokenKind::Unknown })
            }
        }
    }
}

// ———————————————————————————————————————————————————————————————————————
// Preprocessor (#define macros)
// ———————————————————————————————————————————————————————————————————————

/// A macro definition: parameter names and body tokens.
struct MacroDef<'a> {
    /// Whether this is a function-like macro (invoked with parentheses).
    is_fn: bool,
    params: Vec<&'a str>,
    body: Vec<Token<'a>>,
}

/// Builtin macros available in all assembly sources.
fn builtin_macros() -> HashMap<&'static str, MacroDef<'static>> {
    let mut m = HashMap::default();
    m.insert(
        "RET_WORD",
        MacroDef {
            is_fn: false,
            params: vec![],
            body: vec![
                Token { src: "PUSH0", kind: TokenKind::Ident },
                Token { src: "MSTORE", kind: TokenKind::Ident },
                Token { src: "PUSH1", kind: TokenKind::Ident },
                Token { src: "0x20", kind: TokenKind::Number(U256::from(0x20)) },
                Token { src: "PUSH0", kind: TokenKind::Ident },
                Token { src: "RETURN", kind: TokenKind::Ident },
            ],
        },
    );
    m
}

/// Preprocess source text: extract `#define` directives (line-scoped), tokenize the rest,
/// then expand macro invocations on the token stream.
fn preprocess(s: &str) -> Result<Vec<Token<'_>>> {
    let mut macros = builtin_macros();

    // Extract #define lines (tokenize their bodies in-place); keep remaining lines.
    // Note: `#define` bodies borrow from `s` since their source text lives in `s`.
    let mut rest_start = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("#define")
            && (after.is_empty() || after.starts_with(|c: char| c.is_ascii_whitespace()))
        {
            parse_define(after, &mut macros)?;
        } else {
            // Record (start, end) byte offsets into `s` for non-directive lines.
            let offset = trimmed.as_ptr() as usize - s.as_ptr() as usize;
            rest_start.push((offset, offset + trimmed.len()));
        }
    }

    // Tokenize non-directive lines (borrowing from `s`), skipping whitespace and comments.
    let mut raw = Vec::new();
    for &(start, end) in &rest_start {
        let line = &s[start..end];
        raw.extend(
            Tokenizer::new(line)
                .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment)),
        );
    }

    if macros.is_empty() {
        return Ok(raw);
    }

    expand_macros(raw, &macros)
}

/// Parse a `#define` directive body (everything after `#define`) into the macro table.
fn parse_define<'a>(after: &'a str, macros: &mut HashMap<&'a str, MacroDef<'a>>) -> Result<()> {
    let mut tok = Tokenizer::new(after)
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .peekable();

    let name = match tok.next() {
        Some(Token { src, kind: TokenKind::Ident }) => src,
        Some(other) => return Err(eyre::eyre!("expected macro name after #define, got {other:?}")),
        None => return Err(eyre::eyre!("expected macro name after #define")),
    };

    // Function-like macro: NAME(a, b).
    // Only if '(' immediately follows the name (no whitespace), matching C preprocessor semantics.
    let is_fn = after.as_bytes().get(name.as_ptr() as usize - after.as_ptr() as usize + name.len())
        == Some(&b'(');

    let all_tokens: Vec<Token<'a>> = tok.collect();
    let mut i = 0;

    let mut params = Vec::new();
    if is_fn && matches!(all_tokens.get(i), Some(Token { kind: TokenKind::LParen, .. })) {
        i += 1; // consume '('
        if !matches!(all_tokens.get(i), Some(Token { kind: TokenKind::RParen, .. })) {
            loop {
                match all_tokens.get(i) {
                    Some(Token { src: p, kind: TokenKind::Ident }) => {
                        params.push(*p);
                        i += 1;
                    }
                    other => {
                        return Err(eyre::eyre!(
                            "expected parameter name in #define {name}, got {other:?}"
                        ));
                    }
                }
                match all_tokens.get(i) {
                    Some(Token { kind: TokenKind::RParen, .. }) => {
                        i += 1;
                        break;
                    }
                    Some(Token { kind: TokenKind::Comma, .. }) => i += 1,
                    other => {
                        return Err(eyre::eyre!(
                            "expected ',' or ')' in #define {name} parameter list, got {other:?}"
                        ));
                    }
                }
            }
        } else {
            i += 1; // consume ')'
        }
    }

    let body = all_tokens[i..].to_vec();
    macros.insert(name, MacroDef { is_fn, params, body });
    Ok(())
}

/// Expand macro invocations in a token stream.
fn expand_macros<'a>(
    tokens: Vec<Token<'a>>,
    macros: &HashMap<&str, MacroDef<'a>>,
) -> Result<Vec<Token<'a>>> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut iter = tokens.into_iter().peekable();

    while let Some(tok) = iter.next() {
        let TokenKind::Ident = &tok.kind else {
            out.push(tok);
            continue;
        };
        let Some(mac) = macros.get(tok.src) else {
            out.push(tok);
            continue;
        };
        let name = tok.src;

        if !mac.is_fn {
            // Object-like macro: simple body substitution.
            out.extend(mac.body.iter().cloned());
        } else {
            // Function-like macro: consume `(arg1, arg2, ...)`.
            eyre::ensure!(
                matches!(iter.next(), Some(Token { kind: TokenKind::LParen, .. })),
                "macro {name:?} expects arguments",
            );

            // Parse arguments, handling nested parens.
            let mut args: Vec<Vec<Token<'a>>> = vec![vec![]];
            let mut depth = 1u32;
            loop {
                let t = iter
                    .next()
                    .ok_or_else(|| eyre::eyre!("unclosed '(' in macro invocation {name:?}"))?;
                match &t.kind {
                    TokenKind::LParen => {
                        depth += 1;
                        args.last_mut().unwrap().push(t);
                    }
                    TokenKind::RParen => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        args.last_mut().unwrap().push(t);
                    }
                    TokenKind::Comma if depth == 1 => args.push(vec![]),
                    _ => args.last_mut().unwrap().push(t),
                }
            }

            // Zero-arg function-like: `FOO()` produces args = [[]]; expect 0.
            if mac.params.is_empty() {
                eyre::ensure!(
                    args.len() == 1 && args[0].is_empty(),
                    "macro {name:?} takes no arguments"
                );
            } else {
                eyre::ensure!(
                    args.len() == mac.params.len(),
                    "macro {name:?} expects {} argument(s), got {}",
                    mac.params.len(),
                    args.len()
                );
            }

            // Substitute $param refs in the body.
            for body_tok in &mac.body {
                if let TokenKind::ParamRef = body_tok.kind
                    && let Some(idx) = mac.params.iter().position(|p| *p == body_tok.src)
                {
                    out.extend(args[idx].iter().cloned());
                } else {
                    out.push(body_tok.clone());
                }
            }
        }
    }
    Ok(out)
}

// ———————————————————————————————————————————————————————————————————————
// Parser (tokens → items)
// ———————————————————————————————————————————————————————————————————————

/// A parsed item from the source.
enum Item<'a> {
    /// A label definition (`name:`).
    Label(&'a str),
    /// An instruction.
    Inst(Inst<'a>),
}

/// A parsed instruction.
struct Inst<'a> {
    opcode: u8,
    imm: Option<Imm<'a>>,
    push_kind: PushKind,
}

/// An immediate value.
enum Imm<'a> {
    /// A numeric literal.
    Number(U256),
    /// A label reference (resolved during layout).
    Label(&'a str),
}

/// How the push width is determined.
enum PushKind {
    /// Not a push instruction.
    None,
    /// Fixed width (`PUSH1`..`PUSH32`).
    Fixed(u8),
    /// Auto-sized (`PUSH`).
    Auto,
}

/// Parse a token stream into items.
fn parse_items<'a>(tokens: &[Token<'a>]) -> Result<Vec<Item<'a>>> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i].kind {
            TokenKind::Label => {
                let name = tokens[i].src;
                eyre::ensure!(!name.is_empty(), "empty label name");
                items.push(Item::Label(name));
                i += 1;
            }
            TokenKind::Ident => {
                let word = tokens[i].src;
                if word == "PUSH" {
                    i += 1;
                    let imm = expect_imm(tokens, &mut i, "PUSH")?;
                    items.push(Item::Inst(Inst {
                        opcode: 0,
                        imm: Some(imm),
                        push_kind: PushKind::Auto,
                    }));
                } else if word == "DUP" || word == "SWAP" {
                    let is_swap = word == "SWAP";
                    i += 1;
                    let n = expect_number_u8(tokens, &mut i, word)?;
                    eyre::ensure!(n >= 1, "{word} index must be >= 1, got {n}");
                    if n <= 16 {
                        let base = if is_swap { op::SWAP1 } else { op::DUP1 };
                        items.push(Item::Inst(Inst {
                            opcode: base + n - 1,
                            imm: None,
                            push_kind: PushKind::None,
                        }));
                    } else {
                        let eof_op = if is_swap { op::SWAPN } else { op::DUPN };
                        let raw = encode_single(n).ok_or_else(|| {
                            eyre::eyre!("{word} index {n} out of valid range [1, 235]")
                        })?;
                        items.push(Item::Inst(Inst {
                            opcode: eof_op,
                            imm: Some(Imm::Number(U256::from(raw))),
                            push_kind: PushKind::Fixed(1),
                        }));
                    }
                } else {
                    let opc = OpCode::parse(word)
                        .ok_or_else(|| eyre::eyre!("invalid opcode: {word:?}"))?;
                    let opcode = opc.get();
                    i += 1;

                    if opcode == op::DUPN || opcode == op::SWAPN {
                        let n = expect_number_u8(tokens, &mut i, opc)?;
                        let raw = encode_single(n).ok_or_else(|| {
                            eyre::eyre!("{opc} index {n} out of valid range [17, 235]")
                        })?;
                        items.push(Item::Inst(Inst {
                            opcode,
                            imm: Some(Imm::Number(U256::from(raw))),
                            push_kind: PushKind::Fixed(1),
                        }));
                    } else if opcode == op::EXCHANGE {
                        let n = expect_number_u8(tokens, &mut i, opc)?;
                        let m = expect_number_u8(tokens, &mut i, opc)?;
                        let raw = encode_pair(n, m).ok_or_else(|| {
                            eyre::eyre!("EXCHANGE pair ({n}, {m}) cannot be encoded")
                        })?;
                        items.push(Item::Inst(Inst {
                            opcode,
                            imm: Some(Imm::Number(U256::from(raw))),
                            push_kind: PushKind::Fixed(1),
                        }));
                    } else {
                        let imm_len = opc.info().immediate_size();
                        if imm_len > 0 {
                            let imm = expect_imm(tokens, &mut i, opc)?;
                            items.push(Item::Inst(Inst {
                                opcode,
                                imm: Some(imm),
                                push_kind: PushKind::Fixed(imm_len),
                            }));
                        } else {
                            if matches!(
                                tokens.get(i),
                                Some(Token { kind: TokenKind::Number(_), .. })
                            ) {
                                eyre::bail!("unexpected immediate for opcode {opc}");
                            }
                            items.push(Item::Inst(Inst {
                                opcode,
                                imm: None,
                                push_kind: PushKind::None,
                            }));
                        }
                    }
                }
            }
            TokenKind::Unknown => return Err(eyre::eyre!("unexpected token: {:?}", tokens[i].src)),
            _ => return Err(eyre::eyre!("unexpected token: {:?}", tokens[i])),
        }
    }
    Ok(items)
}

/// Consume the next token as a number and convert to u8.
fn expect_number_u8(
    tokens: &[Token<'_>],
    i: &mut usize,
    ctx: impl std::fmt::Display,
) -> Result<u8> {
    let tok = tokens.get(*i).ok_or_else(|| eyre::eyre!("missing immediate for opcode {ctx}"))?;
    *i += 1;
    match &tok.kind {
        TokenKind::Number(n) => {
            let v: u64 =
                n.try_into().map_err(|_| eyre::eyre!("invalid {ctx} immediate: too large"))?;
            u8::try_from(v).map_err(|_| eyre::eyre!("invalid {ctx} immediate: too large"))
        }
        _ => Err(eyre::eyre!("expected numeric immediate for {ctx}, got {tok:?}")),
    }
}

/// Consume the next token as an immediate (number or label ref).
fn expect_imm<'a>(
    tokens: &[Token<'a>],
    i: &mut usize,
    ctx: impl std::fmt::Display,
) -> Result<Imm<'a>> {
    let tok = tokens.get(*i).ok_or_else(|| eyre::eyre!("missing immediate for opcode {ctx}"))?;
    *i += 1;
    match &tok.kind {
        TokenKind::Number(n) => Ok(Imm::Number(*n)),
        TokenKind::LabelRef => Ok(Imm::Label(tok.src)),
        _ => Err(eyre::eyre!("expected immediate for {ctx}, got {tok:?}")),
    }
}

// ———————————————————————————————————————————————————————————————————————
// Layout and emit
// ———————————————————————————————————————————————————————————————————————

/// Encode a U256 as big-endian bytes with optional fixed size.
fn encode_imm(num: U256, size: Option<u8>) -> Result<Vec<u8>> {
    let mut bytes = num.to_be_bytes_trimmed_vec();
    if let Some(size) = size {
        debug_assert!(size <= 32);
        match bytes.len().cmp(&(size as usize)) {
            Ordering::Less => {
                let extend = size as usize - bytes.len();
                bytes.splice(0..0, std::iter::repeat_n(0, extend));
            }
            Ordering::Equal => {}
            Ordering::Greater => {
                eyre::bail!("expected at most {size} immediate bytes, got {}", bytes.len());
            }
        }
    }
    debug_assert!(bytes.len() <= 32);
    Ok(bytes)
}

/// Compute the minimum push width for a value (0 for zero, 1 for 1..=0xff, etc.).
fn min_push_width(val: usize) -> u8 {
    if val == 0 {
        0
    } else {
        let bits = usize::BITS - val.leading_zeros();
        bits.div_ceil(8) as u8
    }
}

/// Layout items with label resolution (fixed-point for auto-sized label pushes) and emit bytecode.
fn layout_and_emit(items: &[Item<'_>]) -> Result<Vec<u8>> {
    let mut auto_label_indices = Vec::new();
    let mut has_any_label = false;

    for (i, item) in items.iter().enumerate() {
        match item {
            Item::Label(_) => has_any_label = true,
            Item::Inst(inst) => {
                if matches!(inst.imm, Some(Imm::Label(_))) {
                    has_any_label = true;
                    if matches!(inst.push_kind, PushKind::Auto) {
                        auto_label_indices.push(i);
                    }
                }
            }
        }
    }

    // If no labels at all, just emit directly.
    if !has_any_label {
        let mut code = Vec::with_capacity(32);
        for item in items {
            if let Item::Inst(inst) = item {
                emit_inst_no_labels(inst, &mut code)?;
            }
        }
        return Ok(code);
    }

    // Fixed-point layout: auto-push widths start at 0 and grow monotonically.
    let mut auto_widths = vec![0u8; items.len()];
    let mut label_pcs = HashMap::<&str, usize>::default();

    loop {
        label_pcs.clear();
        let mut pc = 0usize;
        for (i, item) in items.iter().enumerate() {
            match item {
                Item::Label(name) => {
                    label_pcs.insert(name, pc);
                }
                Item::Inst(inst) => {
                    pc += 1;
                    match &inst.push_kind {
                        PushKind::None => {}
                        PushKind::Fixed(n) => pc += *n as usize,
                        PushKind::Auto => {
                            let width = match &inst.imm {
                                Some(Imm::Label(_)) => auto_widths[i],
                                Some(Imm::Number(n)) => {
                                    let bytes = n.to_be_bytes_trimmed_vec();
                                    bytes.len() as u8
                                }
                                None => 0,
                            };
                            pc += width as usize;
                        }
                    }
                }
            }
        }

        let mut changed = false;
        for &i in &auto_label_indices {
            if let Item::Inst(inst) = &items[i]
                && let Some(Imm::Label(name)) = &inst.imm
            {
                let target_pc =
                    *label_pcs.get(name).ok_or_else(|| eyre::eyre!("undefined label: {name:?}"))?;
                let needed = min_push_width(target_pc);
                if needed > auto_widths[i] {
                    auto_widths[i] = needed;
                    changed = true;
                }
            }
        }

        if !changed {
            break;
        }
    }

    // Final emit.
    let mut code = Vec::with_capacity(64);
    for (i, item) in items.iter().enumerate() {
        if let Item::Inst(inst) = item {
            match &inst.push_kind {
                PushKind::None => {
                    code.push(inst.opcode);
                }
                PushKind::Fixed(size) => {
                    code.push(inst.opcode);
                    let val = resolve_imm(inst.imm.as_ref().unwrap(), &label_pcs)?;
                    let bytes = encode_imm(val, Some(*size))?;
                    code.extend_from_slice(&bytes);
                }
                PushKind::Auto => {
                    let val = resolve_imm(inst.imm.as_ref().unwrap(), &label_pcs)?;
                    let width = match &inst.imm {
                        Some(Imm::Label(_)) => auto_widths[i],
                        _ => {
                            let bytes = val.to_be_bytes_trimmed_vec();
                            bytes.len() as u8
                        }
                    };
                    let push0 = OpCode::PUSH0.get();
                    code.push(push0 + width);
                    if width > 0 {
                        let bytes = encode_imm(val, Some(width))?;
                        code.extend_from_slice(&bytes);
                    }
                }
            }
        }
    }

    Ok(code)
}

/// Resolve an immediate value, substituting label PCs.
fn resolve_imm(imm: &Imm<'_>, label_pcs: &HashMap<&str, usize>) -> Result<U256> {
    match imm {
        Imm::Number(n) => Ok(*n),
        Imm::Label(name) => {
            let pc = label_pcs.get(name).ok_or_else(|| eyre::eyre!("undefined label: {name:?}"))?;
            Ok(U256::from(*pc))
        }
    }
}

/// Emit a single instruction (no-label fast path).
fn emit_inst_no_labels(inst: &Inst<'_>, code: &mut Vec<u8>) -> Result<()> {
    match &inst.push_kind {
        PushKind::None => {
            code.push(inst.opcode);
        }
        PushKind::Fixed(size) => {
            code.push(inst.opcode);
            let Imm::Number(n) = inst.imm.as_ref().unwrap() else {
                unreachable!();
            };
            let bytes = encode_imm(*n, Some(*size))?;
            code.extend_from_slice(&bytes);
        }
        PushKind::Auto => {
            let Imm::Number(n) = inst.imm.as_ref().unwrap() else {
                unreachable!();
            };
            let bytes = encode_imm(*n, None)?;
            let push0 = OpCode::PUSH0.get();
            code.push(push0 + bytes.len() as u8);
            code.extend_from_slice(&bytes);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;

    use super::*;

    #[test]
    fn basic_opcodes() {
        let cases: &[(&str, Vec<u8>)] = &[
            ("ADD ; ADD\n ADD", vec![op::ADD, op::ADD]),
            ("PUSH1 0", vec![op::PUSH1, 0]),
            ("PUSH3 0x000069", vec![op::PUSH3, 0, 0, 0x69]),
            ("PUSH3 0x69 ; padded", vec![op::PUSH3, 0, 0, 0x69]),
            ("PUSH 0", vec![op::PUSH0]),
            ("PUSH 1", vec![op::PUSH1, 1]),
            ("PUSH 2", vec![op::PUSH1, 2]),
            ("PUSH 69", vec![op::PUSH1, 69]),
            ("PUSH 0x2222", vec![op::PUSH2, 0x22, 0x22]),
        ];
        for (s, expected) in cases.iter() {
            let code = match parse_asm(s) {
                Ok(code) => code,
                Err(e) => panic!("code: {s:?}\n\n err: {e}"),
            };
            assert_eq!(code, *expected, "{s:?}");
        }
    }

    #[test]
    fn label_forward_ref() {
        let code = parse_asm(
            "
            PUSH %target
            JUMP
        target:
            JUMPDEST
            STOP
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::PUSH1, 3, op::JUMP, op::JUMPDEST, op::STOP]);
    }

    #[test]
    fn label_backward_ref() {
        let code = parse_asm(
            "
        target:
            JUMPDEST
            PUSH %target
            JUMP
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::JUMPDEST, op::PUSH0, op::JUMP]);
    }

    #[test]
    fn label_fixed_width() {
        let code = parse_asm(
            "
            PUSH1 %target
            JUMP
        target:
            JUMPDEST
            STOP
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::PUSH1, 3, op::JUMP, op::JUMPDEST, op::STOP]);
    }

    #[test]
    fn multiple_labels_same_pc() {
        let code = parse_asm(
            "
        a:
        b:
            JUMPDEST
            PUSH %a
            PUSH %b
            STOP
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::JUMPDEST, op::PUSH0, op::PUSH0, op::STOP]);
    }

    #[test]
    fn dup_auto() {
        // DUP 1..16 → DUP1..DUP16 (no immediate).
        assert_eq!(parse_asm("DUP 1").unwrap(), vec![op::DUP1]);
        assert_eq!(parse_asm("DUP 16").unwrap(), vec![op::DUP16]);
        // DUP 17+ → DUPN with encoded immediate.
        assert_eq!(parse_asm("DUP 17").unwrap(), vec![op::DUPN, 0x80]);
        assert_eq!(parse_asm("DUP 108").unwrap(), vec![op::DUPN, 0xDB]);
        // DUP 0 is invalid.
        assert!(parse_asm("DUP 0").is_err());
        // DUP 236 is out of range.
        assert!(parse_asm("DUP 236").is_err());
    }

    #[test]
    fn swap_auto() {
        // SWAP 1..16 → SWAP1..SWAP16 (no immediate).
        assert_eq!(parse_asm("SWAP 1").unwrap(), vec![op::SWAP1]);
        assert_eq!(parse_asm("SWAP 16").unwrap(), vec![op::SWAP16]);
        // SWAP 17+ → SWAPN with encoded immediate.
        assert_eq!(parse_asm("SWAP 17").unwrap(), vec![op::SWAPN, 0x80]);
        assert_eq!(parse_asm("SWAP 108").unwrap(), vec![op::SWAPN, 0xDB]);
        // SWAP 0 is invalid.
        assert!(parse_asm("SWAP 0").is_err());
    }

    #[test]
    fn dupn() {
        // Explicit DUPN only accepts 17+.
        assert_eq!(parse_asm("DUPN 17").unwrap(), vec![op::DUPN, 0x80]);
        assert_eq!(parse_asm("DUPN 108").unwrap(), vec![op::DUPN, 0xDB]);
        assert!(parse_asm("DUPN 16").is_err());
        assert!(parse_asm("DUPN 0").is_err());
        assert!(parse_asm("DUPN 236").is_err());
        assert!(parse_asm("DUPN abc").is_err());
    }

    #[test]
    fn swapn() {
        assert_eq!(parse_asm("SWAPN 17").unwrap(), vec![op::SWAPN, 0x80]);
        assert_eq!(parse_asm("SWAPN 108").unwrap(), vec![op::SWAPN, 0xDB]);
        assert!(parse_asm("SWAPN 16").is_err());
        assert!(parse_asm("SWAPN 0").is_err());
    }

    #[test]
    fn exchange() {
        // Two separate number tokens.
        assert_eq!(parse_asm("EXCHANGE 1 2").unwrap(), vec![op::EXCHANGE, 0x8E]);
        assert!(parse_asm("EXCHANGE 1 14").is_ok());
        // (2, 1) cannot be encoded.
        assert!(parse_asm("EXCHANGE 2 1").is_err());
        // (0, 1) is invalid (zero index).
        assert!(parse_asm("EXCHANGE 0 1").is_err());
        // Missing second operand.
        assert!(parse_asm("EXCHANGE 1").is_err());
        // Non-numeric.
        assert!(parse_asm("EXCHANGE a b").is_err());
    }

    #[test]
    fn slotnum() {
        assert_eq!(parse_asm("SLOTNUM").unwrap(), vec![op::SLOTNUM]);
    }

    #[test]
    fn undefined_label() {
        assert!(parse_asm("PUSH %missing JUMP").is_err());
    }

    #[test]
    fn empty_label() {
        assert!(parse_asm(": STOP").is_err());
    }

    #[test]
    fn empty_label_ref() {
        assert!(parse_asm("PUSH % JUMP").is_err());
    }

    #[test]
    fn define_macro() {
        let code = parse_asm(
            "
            #define TWO PUSH 2
            TWO
            TWO
            ADD
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::PUSH1, 2, op::PUSH1, 2, op::ADD]);
    }

    #[test]
    fn builtin_ret_word() {
        let code = parse_asm("CALLVALUE RET_WORD").unwrap();
        assert_eq!(
            code,
            vec![op::CALLVALUE, op::PUSH0, op::MSTORE, op::PUSH1, 0x20, op::PUSH0, op::RETURN]
        );
    }

    #[test]
    fn define_override_builtin() {
        let code = parse_asm(
            "
            #define RET_WORD STOP
            RET_WORD
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::STOP]);
    }

    #[test]
    fn define_with_args() {
        let code = parse_asm(
            "
            #define PUSH_TWO(a, b) PUSH $a PUSH $b
            PUSH_TWO(1, 2)
            ADD
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::PUSH1, 1, op::PUSH1, 2, op::ADD]);
    }

    #[test]
    fn define_single_arg() {
        let code = parse_asm(
            "
            #define PUSH_AND_STORE(val) PUSH $val PUSH0 MSTORE
            PUSH_AND_STORE(0x42)
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::PUSH1, 0x42, op::PUSH0, op::MSTORE]);
    }

    #[test]
    fn define_missing_args() {
        assert!(
            parse_asm(
                "
            #define FOO(a) PUSH $a
            FOO
        "
            )
            .is_err()
        );
    }

    #[test]
    fn define_wrong_arg_count() {
        assert!(
            parse_asm(
                "
            #define FOO(a, b) PUSH $a PUSH $b
            FOO(1)
        "
            )
            .is_err()
        );
    }

    #[test]
    fn define_empty_name() {
        assert!(parse_asm("#define").is_err());
        assert!(parse_asm("#define  ").is_err());
    }

    #[test]
    fn define_invalid_name() {
        // `-` is tokenized as Unknown, so it can't be a macro name.
        assert!(parse_asm("#define - STOP").is_err());
        // `#define` with no name at all.
        assert!(parse_asm("#define (a) STOP").is_err());
    }

    #[test]
    fn define_no_space_after_keyword() {
        // `#defineFOO` should not be treated as a directive.
        assert!(parse_asm("#defineFOO STOP").is_err());
    }

    #[test]
    fn define_zero_arg_fn() {
        let code = parse_asm(
            "
            #define NOP() ADD
            NOP()
        ",
        )
        .unwrap();
        assert_eq!(code, vec![op::ADD]);
    }

    #[test]
    fn define_zero_arg_fn_bare_is_error() {
        // Function-like macro requires parentheses.
        assert!(
            parse_asm(
                "
            #define NOP() ADD
            NOP
        "
            )
            .is_err()
        );
    }

    #[test]
    fn define_zero_arg_fn_with_args_is_error() {
        assert!(
            parse_asm(
                "
            #define NOP() ADD
            NOP(1)
        "
            )
            .is_err()
        );
    }

    #[test]
    fn define_malformed_param_list() {
        // Missing closing paren.
        assert!(parse_asm("#define FOO(a STOP").is_err());
        // Missing comma between params.
        assert!(parse_asm("#define FOO(a b) STOP").is_err());
        // Number as param name.
        assert!(parse_asm("#define FOO(1) STOP").is_err());
        // Trailing comma.
        assert!(parse_asm("#define FOO(a,) STOP").is_err());
    }

    #[test]
    fn define_space_before_paren_is_object_like() {
        // `#define FOO (a) STOP` — space before `(` makes it object-like with body `(a) STOP`.
        // Using FOO should expand to `(a) STOP`, and `(` is not a valid opcode.
        assert!(
            parse_asm(
                "
            #define FOO (a) STOP
            FOO
        "
            )
            .is_err()
        );
    }
}
