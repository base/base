#![cfg(feature = "t4b-shadow")]
#![doc = "Compile and source-structure seal for the real checked-bindings production chain."]

use std::{collections::BTreeSet, fs, path::PathBuf};

use mev_trader_submit::{
    CandidateExecutionAdapter, CheckedBindings, CheckedBindingsView, TxAuthorityExecutionParts,
    TxAuthorityExecutionRequest,
};

const VIEW_METHODS: [&str; 26] = [
    "access_digest",
    "beryl_env",
    "deployment_witness",
    "executor",
    "frame",
    "frame_digest",
    "freshness_witness",
    "header_coinbase",
    "header_identity_digest",
    "kickback_recipient",
    "nonce_witness",
    "order_digest",
    "overlay_digest",
    "parent_hash",
    "parent_header",
    "plan_digest",
    "resolved_adapters",
    "route_digest",
    "route_hops",
    "route_pools",
    "route_protocols",
    "route_tokens",
    "sender",
    "shape_digest",
    "state_digest",
    "unsigned_signing_hash",
];
const BERYL_METHODS: [&str; 7] = [
    "base_fee_per_gas",
    "block_number",
    "chain_id",
    "excess_blob_gas",
    "gas_limit",
    "prev_randao",
    "timestamp",
];
const DEPLOYMENT_METHODS: [&str; 3] = ["executor", "route_adapters", "validated_parent"];
const NONCE_METHODS: [&str; 5] =
    ["committed_nonce", "parent_hash", "pending_overlay_nonce", "sender", "shape_nonce"];
const FRESHNESS_METHODS: [&str; 4] =
    ["parent_hash", "snapshot_identity_digest", "snapshot_parent_hash", "valid_until_block"];

fn read(relative: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let path = root.join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn rust_without_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut block_depth = 0;
    let mut line_comment = false;
    let mut string = false;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if line_comment {
            output.push(if byte == b'\n' { b'\n' } else { b' ' });
            line_comment = byte != b'\n';
            index += 1;
        } else if block_depth > 0 {
            if byte == b'/' && next == Some(b'*') {
                output.extend_from_slice(b"  ");
                block_depth += 1;
                index += 2;
            } else if byte == b'*' && next == Some(b'/') {
                output.extend_from_slice(b"  ");
                block_depth -= 1;
                index += 2;
            } else {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
        } else if string {
            if byte == b'\\' && next.is_some() {
                output.extend_from_slice(b"  ");
                index += 2;
            } else {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                string = byte != b'"';
                index += 1;
            }
        } else if byte == b'/' && next == Some(b'/') {
            output.extend_from_slice(b"  ");
            line_comment = true;
            index += 2;
        } else if byte == b'/' && next == Some(b'*') {
            output.extend_from_slice(b"  ");
            block_depth = 1;
            index += 2;
        } else if byte == b'"' {
            output.push(b' ');
            string = true;
            index += 1;
        } else {
            output.push(byte);
            index += 1;
        }
    }
    String::from_utf8(output).expect("source was UTF-8")
}

fn item_block<'a>(source: &'a str, marker: &str) -> &'a str {
    let start = source.find(marker).unwrap_or_else(|| panic!("missing production item: {marker}"));
    let opening = source[start..].find('{').map(|offset| start + offset).expect("item body");
    let mut depth = 0;
    for (offset, byte) in source.as_bytes()[opening..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[start..=opening + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unclosed production item: {marker}")
}

fn public_methods(source: &str, marker: &str) -> BTreeSet<String> {
    let body = item_block(source, marker);
    body.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let rest =
                line.strip_prefix("pub const fn ").or_else(|| line.strip_prefix("pub fn "))?;
            rest.split_once('(').map(|(name, _)| name.trim().to_owned())
        })
        .collect()
}

fn assert_calls(context: &str, receiver: &str, methods: &[&str]) {
    for method in methods {
        let call = format!("{receiver}.{method}(");
        assert!(context.contains(&call), "operation context lost `{call}`");
    }
}

fn compile_public_linear_api_without_constructing_private_types() {
    fn public_type<T>() {}
    fn public_lifetime_type<T>() {}
    fn adapter_trait<A: CandidateExecutionAdapter>() {}
    let _ = public_type::<CheckedBindings>;
    let _ = public_lifetime_type::<CheckedBindingsView<'static>>;
    let _ = public_lifetime_type::<TxAuthorityExecutionRequest<'static>>;
    let _ = public_lifetime_type::<TxAuthorityExecutionParts<'static>>;
    let _ = adapter_trait::<NeverConstructedAdapter>;
}

enum NeverConstructedAdapter {}

impl CandidateExecutionAdapter for NeverConstructedAdapter {
    type Error = core::convert::Infallible;

    fn execute_candidate(
        self,
        _request: TxAuthorityExecutionRequest<'_>,
    ) -> Result<mev_trader_submit::CandidateEconomicsEvidence, Self::Error> {
        match self {}
    }
}

#[test]
fn real_production_chain_and_checked_projection_are_exact() {
    compile_public_linear_api_without_constructing_private_types();
    let cli = rust_without_comments_and_strings(&read("crates/execution/cli/src/mev_trader.rs"));
    let submit = rust_without_comments_and_strings(&read(
        "crates/execution/mev-trader-submit/src/tx_authority.rs",
    ));

    assert_eq!(
        public_methods(&submit, "impl<'a> CheckedBindingsView<'a>"),
        VIEW_METHODS.into_iter().map(str::to_owned).collect()
    );
    for (marker, expected) in [
        ("impl CheckedBerylEnvInputs", &BERYL_METHODS[..]),
        ("impl DeploymentWitness", &DEPLOYMENT_METHODS[..]),
        ("impl NonceWitness", &NONCE_METHODS[..]),
        ("impl FreshnessWitness", &FRESHNESS_METHODS[..]),
    ] {
        assert_eq!(
            public_methods(&submit, marker),
            expected.iter().copied().map(str::to_owned).collect(),
            "nested getter ABI changed at {marker}"
        );
    }

    let observer = item_block(
        &cli,
        "impl<Provider> CandidateTxShapeObserver for T4bShadowAuthority<Provider>",
    );
    let observe = item_block(observer, "fn try_observe");
    assert!(observe.contains("fn try_observe(&self, view: &CandidateAssemblyView<'_>)"));
    let body = observe.split_once('{').expect("observer body").1.trim_start();
    assert!(
        body.starts_with("let pre = match self.assembler.prepare_pre_economics(view)"),
        "prepare_pre_economics(view) is not the first production statement"
    );
    assert_eq!(observe.matches("T4bParentOverlayAdapter::new(").count(), 1);
    assert_eq!(observe.matches(".execute_once(").count(), 1);
    assert!(!observe.contains("assemble_sealed("));
    assert!(!observe.contains(".assemble("));

    let adapter = item_block(
        &cli,
        "impl<Provider> CandidateExecutionAdapter for T4bParentOverlayAdapter<Provider>",
    );
    let execute = item_block(adapter, "fn execute_candidate");
    assert!(execute.contains("request.into_parts()"));
    assert!(execute.contains("parts.into_tx_and_bindings()"));
    assert_eq!(execute.matches("evm.transact(").count(), 1);

    assert_calls(
        execute,
        "bindings",
        &[
            "frame",
            "parent_hash",
            "beryl_env",
            "parent_header",
            "header_identity_digest",
            "sender",
            "kickback_recipient",
            "header_coinbase",
            "deployment_witness",
            "nonce_witness",
            "freshness_witness",
            "executor",
            "resolved_adapters",
            "route_hops",
            "route_pools",
            "route_tokens",
            "route_protocols",
        ],
    );
    let evidence = item_block(&submit, "impl CandidateEconomicsEvidence");
    assert_calls(
        evidence,
        "bindings",
        &[
            "frame_digest",
            "plan_digest",
            "route_digest",
            "shape_digest",
            "overlay_digest",
            "order_digest",
            "state_digest",
            "access_digest",
            "unsigned_signing_hash",
        ],
    );
    assert_calls(execute, "beryl", &BERYL_METHODS);
    assert_calls(execute, "deployment", &DEPLOYMENT_METHODS);
    assert_calls(execute, "nonce", &NONCE_METHODS);
    assert_calls(execute, "freshness", &FRESHNESS_METHODS);
}
