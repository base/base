//! Source seal proving P1 adds no Commit-B hook, task, or authority.
#![cfg(feature = "b5-dormant-presign")]

const MEV_TRADER_SOURCE: &str = include_str!("../src/mev_trader.rs");
const B5_DORMANT_SOURCE: &str = include_str!("../src/mev_trader/b5_dormant.rs");

fn source_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = String::with_capacity(source.len());
    let mut index = 0;
    let mut block_depth = 0usize;
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if in_line_comment {
            if byte == b'\n' {
                in_line_comment = false;
                code.push('\n');
            } else {
                code.push(' ');
            }
        } else if block_depth != 0 {
            if byte == b'/' && next == Some(b'*') {
                block_depth += 1;
                code.push_str("  ");
                index += 1;
            } else if byte == b'*' && next == Some(b'/') {
                block_depth -= 1;
                code.push_str("  ");
                index += 1;
            } else if byte == b'\n' {
                code.push('\n');
            } else {
                code.push(' ');
            }
        } else if in_string {
            if byte == b'\n' {
                code.push('\n');
            } else {
                code.push(' ');
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'/' && next == Some(b'/') {
            in_line_comment = true;
            code.push_str("  ");
            index += 1;
        } else if byte == b'/' && next == Some(b'*') {
            block_depth = 1;
            code.push_str("  ");
            index += 1;
        } else if byte == b'"' {
            in_string = true;
            code.push(' ');
        } else {
            code.push(char::from(byte));
        }
        index += 1;
    }

    assert_eq!(block_depth, 0, "unterminated block comment in inspected source");
    assert!(!in_string, "unterminated string in inspected source");
    code
}

fn production_code(source: &str) -> String {
    let code = source_code(source);
    code.split("#[cfg(test)]").next().expect("source prefix").to_owned()
}

fn occurrences(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}

fn declared_constant_names(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = ["pub(super) ", "pub(crate) ", "pub "]
                .into_iter()
                .find_map(|visibility| line.strip_prefix(visibility))
                .unwrap_or(line);
            if line.starts_with("const fn ") {
                return None;
            }
            let declaration =
                line.strip_prefix("const ").or_else(|| line.strip_prefix("static "))?;
            declaration
                .split(|character: char| !(character == '_' || character.is_ascii_alphanumeric()))
                .next()
        })
        .collect()
}

#[test]
fn dormant_child_is_the_only_cfg_gated_module_addition() {
    let code = source_code(MEV_TRADER_SOURCE);
    let lines: Vec<_> = MEV_TRADER_SOURCE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect();
    let declarations: Vec<_> =
        lines.iter().enumerate().filter(|(_, line)| **line == "mod b5_dormant;").collect();

    assert_eq!(declarations.len(), 1, "expected exactly one private dormant child declaration");
    let (index, _) = declarations[0];
    assert_eq!(lines[index - 1], "#[cfg(feature = \"b5-dormant-presign\")]");
    assert_eq!(occurrences(&code, "pub mod b5_dormant"), 0);
    assert_eq!(occurrences(&code, "pub(crate) mod b5_dormant"), 0);
}

#[test]
fn commit_b_verifier_call_wrapper_and_authority_are_absent() {
    let mev = production_code(MEV_TRADER_SOURCE);
    let dormant = production_code(B5_DORMANT_SOURCE);

    assert_eq!(occurrences(&mev, "verify_provisioning_bindings_against"), 0);
    assert_eq!(occurrences(&mev, "verify_provisioning_bindings("), 0);
    assert_eq!(occurrences(&dormant, "fn verify_provisioning_bindings("), 0);
    assert_eq!(declared_constant_names(&mev), Vec::<&str>::new());
    assert_eq!(
        declared_constant_names(&dormant),
        ["BOUND_CHAIN_ID", "PINNED_SOURCE_COMMIT", "_"],
        "a production const/static was added to the P1 child"
    );
}

#[test]
fn existing_start_idle_and_extension_side_effect_topology_is_sealed() {
    let code = production_code(MEV_TRADER_SOURCE);
    let expected_inventory = [
        ("start_idle", 2),
        ("subscribe_to_flashblocks", 1),
        ("MevTraderRuntime::start", 1),
        ("BlinkFeedClient::new", 1),
        ("broadcast::Receiver", 1),
        ("spawn_with_graceful_shutdown_signal", 1),
        ("tokio::spawn", 4),
        ("run_consumer", 1),
        ("run_control", 1),
        (".write(", 1),
        ("channel(", 0),
        ("network", 0),
        (".send(", 0),
        ("TcpStream", 0),
        ("UdpSocket", 0),
        ("Network", 0),
        ("OpenOptions", 0),
        ("File::create", 0),
        ("fs::write", 0),
        ("write_all", 0),
        ("b5_dormant::", 0),
    ];

    for (symbol, expected) in expected_inventory {
        assert_eq!(occurrences(&code, symbol), expected, "topology changed at `{symbol}`");
    }
}
