#![doc = "Canonical unsigned envelope and upstream L1-fee authority tests."]
use std::{
    collections::BTreeSet,
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_common_evm::{BaseSpecId, L1BlockInfo};
use mev_trader_submit::{
    CandidateEconomicsEvidence, CanonicalEnvelopeFactory, MAX_CANONICAL_ENVELOPE_LEN,
    TxAuthorityError,
};
use syn::{Expr, ImplItem, Item, Stmt, visit::Visit};

struct FinalizeCallInventory {
    evaluated_terminal: usize,
    ledger_append: usize,
}

impl<'ast> Visit<'ast> for FinalizeCallInventory {
    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if matches!(
            expression.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.last().is_some_and(|segment| segment.ident == "evaluated_terminal")
        ) {
            self.evaluated_terminal += 1;
        }
        syn::visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        if expression.method == "append"
            && matches!(
                expression.receiver.as_ref(),
                Expr::Path(path) if path.path.is_ident("ledger")
            )
        {
            self.ledger_append += 1;
        }
        syn::visit::visit_expr_method_call(self, expression);
    }
}

fn is_priority_economics_clone(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::MethodCall(call)
            if call.method == "clone"
                && call.args.is_empty()
                && matches!(
                    call.receiver.as_ref(),
                    Expr::Path(path) if path.path.is_ident("priority_economics")
                )
    )
}

fn is_sealed_ledger_append(statement: &Stmt) -> bool {
    let Stmt::Expr(Expr::Try(try_expression), Some(_)) = statement else {
        return false;
    };
    let Expr::MethodCall(map_err) = try_expression.expr.as_ref() else {
        return false;
    };
    if map_err.method != "map_err" || map_err.args.len() != 1 {
        return false;
    }
    let Expr::MethodCall(append) = map_err.receiver.as_ref() else {
        return false;
    };
    if append.method != "append"
        || !matches!(append.receiver.as_ref(), Expr::Path(path) if path.path.is_ident("ledger"))
        || append.args.len() != 1
        || !is_priority_economics_clone(&append.args[0])
    {
        return false;
    }
    matches!(
        &map_err.args[0],
        Expr::Closure(closure)
            if matches!(
                closure.body.as_ref(),
                Expr::Path(path)
                    if path.path.segments.len() == 2
                        && path.path.segments[0].ident == "TxAuthorityError"
                        && path.path.segments[1].ident == "PriorityEconomicsLedgerUnavailable"
            )
    )
}

fn tx_authority_finalize_is_sealed(source: &str) -> bool {
    let Ok(file) = syn::parse_file(source) else {
        return false;
    };
    let Some(method) = file.items.iter().find_map(|item| {
        let Item::Impl(item) = item else {
            return None;
        };
        let syn::Type::Path(self_type) = item.self_ty.as_ref() else {
            return None;
        };
        if !self_type
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "TxAuthorityAssembler")
        {
            return None;
        }
        item.items.iter().find_map(|member| match member {
            ImplItem::Fn(method) if method.sig.ident == "finalize" => Some(method),
            _ => None,
        })
    }) else {
        return false;
    };

    let mut calls = FinalizeCallInventory { evaluated_terminal: 0, ledger_append: 0 };
    calls.visit_block(&method.block);
    if calls.evaluated_terminal != 1 || calls.ledger_append != 1 {
        return false;
    }

    let statements = &method.block.stmts;
    let Some(evaluated_index) = statements.iter().position(|statement| {
        matches!(
            statement,
            Stmt::Local(local)
                if matches!(
                    &local.pat,
                    syn::Pat::Ident(binding) if binding.ident == "priority_economics"
                )
                    && matches!(
                        local.init.as_ref().map(|init| init.expr.as_ref()),
                        Some(Expr::Try(try_expression))
                            if matches!(
                                try_expression.expr.as_ref(),
                                Expr::Call(call)
                                    if matches!(
                                        call.func.as_ref(),
                                        Expr::Path(path)
                                            if path.path.segments.last().is_some_and(
                                                |segment| segment.ident == "evaluated_terminal"
                                            )
                                    )
                            )
                    )
        )
    }) else {
        return false;
    };
    let Some(append_index) = statements.iter().position(is_sealed_ledger_append) else {
        return false;
    };
    let Some(candidate_index) = statements.iter().position(|statement| {
        matches!(
            statement,
            Stmt::Expr(Expr::Call(call), None)
                if matches!(
                    call.func.as_ref(),
                    Expr::Path(path) if path.path.is_ident("Ok")
                )
                    && matches!(
                        call.args.first(),
                        Some(Expr::Struct(candidate))
                            if candidate.path.segments.last().is_some_and(
                                |segment| segment.ident == "ValidatedUnsignedAtomicTx"
                            )
                    )
        )
    }) else {
        return false;
    };

    evaluated_index < append_index && append_index < candidate_index
}

fn submit_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn grouped_moved_value_compile_fail_controls() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
    let root = std::env::temp_dir().join(format!("tx-authority-linear-{unique}"));
    fs::create_dir_all(root.join("src/bin")).expect("fixture directory");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname='linear-fixture'\nversion='0.0.0'\nedition='2024'\n\n[dependencies]\nmev-trader-submit={{path='{}',default-features=false,features=['tx-authority']}}\n",
            submit_root().display()
        ),
    ).expect("fixture manifest");
    let fixtures = [
        (
            "request",
            "use mev_trader_submit::TxAuthorityExecutionRequest; fn moved(x: TxAuthorityExecutionRequest<'_>) { let _ = x.into_parts(); let _ = x.into_parts(); } fn main() {}",
        ),
        (
            "parts",
            "use mev_trader_submit::TxAuthorityExecutionParts; fn moved(x: TxAuthorityExecutionParts<'_>) { let _ = x.into_tx_and_bindings(); let _ = x.into_tx_and_bindings(); } fn main() {}",
        ),
        (
            "pre",
            "use mev_trader_submit::{CandidateExecutionAdapter,CandidateEconomicsEvidence,PreEconomicsCandidate,TxAuthorityExecutionRequest}; struct A; impl CandidateExecutionAdapter for A { type Error=(); fn execute_candidate(self,_:TxAuthorityExecutionRequest<'_>)->Result<CandidateEconomicsEvidence,Self::Error>{unreachable!()} } fn moved(x:PreEconomicsCandidate){let _=x.execute_once(A);let _=x.execute_once(A);} fn main(){}",
        ),
        (
            "adapter",
            "use mev_trader_submit::{CandidateExecutionAdapter,CandidateEconomicsEvidence,TxAuthorityExecutionRequest}; struct A; impl CandidateExecutionAdapter for A { type Error=(); fn execute_candidate(self,_:TxAuthorityExecutionRequest<'_>)->Result<CandidateEconomicsEvidence,Self::Error>{unreachable!()} } fn moved(a:A,x:TxAuthorityExecutionRequest<'_>){let _=a.execute_candidate(x);let _=a.execute_candidate(x);} fn main(){}",
        ),
    ];
    for &(name, source) in &fixtures {
        fs::write(root.join(format!("src/bin/{name}.rs")), source).expect("fixture source");
    }
    for &(name, _) in &fixtures {
        let output = Command::new(env!("CARGO"))
            .args(["check", "--offline", "--quiet", "--bin", name])
            .current_dir(&root)
            .output()
            .expect("nested cargo");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name} moved-value fixture unexpectedly compiled");
        assert!(
            stderr.contains("use of moved value") || stderr.contains("borrow of moved value"),
            "{name} failed for an unintended reason: {stderr}"
        );
    }
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn adapter_spy_entry_and_execution_cardinality_are_one_to_one() {
    let source = include_str!("../src/tx_authority.rs");
    let execute_once = source
        .split("impl PreEconomicsCandidate")
        .nth(1)
        .expect("pre impl")
        .split("/// Candidate carrying")
        .next()
        .expect("execute_once body");
    assert_eq!(execute_once.matches("TxAuthorityExecutionRequest::new_private(&self)").count(), 1);
    assert_eq!(execute_once.matches("adapter.execute_candidate(request)").count(), 1);
    assert!(!execute_once.contains("FnOnce"));
    assert_eq!(
        CandidateEconomicsEvidence::checked_weth_delta(U256::from(100), U256::from(140)),
        Ok(U256::from(40)),
    );
    assert_eq!(
        CandidateEconomicsEvidence::checked_weth_delta(U256::from(140), U256::from(100)),
        Err(TxAuthorityError::PriorityEconomicsRejected),
    );
    assert_eq!(
        CandidateEconomicsEvidence::checked_weth_delta(U256::from(100), U256::from(100)),
        Err(TxAuthorityError::PriorityEconomicsRejected),
    );
    assert!(tx_authority_finalize_is_sealed(source));
    let append = "        ledger\n            .append(priority_economics.clone())\n            .map_err(|_| TxAuthorityError::PriorityEconomicsLedgerUnavailable)?;\n";
    let deleted_append = source.replacen(append, "", 1);
    let duplicate_append = source.replacen(append, &format!("{append}{append}"), 1);
    let wrong_mapping = source.replacen(
        ".map_err(|_| TxAuthorityError::PriorityEconomicsLedgerUnavailable)?;",
        ".map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?;",
        1,
    );
    let reordered_append = source
        .replacen(append, "", 1)
        .replacen(
            "        Ok(ValidatedUnsignedAtomicTx {",
            "        Ok({\n            let candidate = ValidatedUnsignedAtomicTx {",
            1,
        )
        .replacen(
            "            priority_economics,\n",
            "            priority_economics: priority_economics.clone(),\n",
            1,
        )
        .replacen(
            "            snapshot_freshness: pre.snapshot_freshness,\n        })\n    }\n\n    fn evaluated_terminal(",
            "            snapshot_freshness: pre.snapshot_freshness,\n            };\n            ledger\n                .append(priority_economics.clone())\n                .map_err(|_| TxAuthorityError::PriorityEconomicsLedgerUnavailable)?;\n            candidate\n        })\n    }\n\n    fn evaluated_terminal(",
            1,
        );
    for (name, mutant) in [
        ("deleted append", deleted_append),
        ("duplicate append", duplicate_append),
        ("append after candidate construction", reordered_append),
        ("wrong append error mapping", wrong_mapping),
    ] {
        syn::parse_file(&mutant)
            .unwrap_or_else(|error| panic!("{name} mutation must parse: {error}"));
        assert!(!tx_authority_finalize_is_sealed(&mutant), "{name} mutation escaped the seal");
    }
    let cli = include_str!("../../cli/src/mev_trader.rs");
    assert!(cli.contains("CandidateEconomicsEvidence::checked_weth_delta(pre_weth, post_weth)"));

    let finalize = source
        .split("pub fn finalize(")
        .nth(1)
        .expect("production finalize")
        .split("fn checkpoint(")
        .next()
        .expect("finalize body");
    assert!(finalize.contains("evidence.weth_delta != decision.kickback_wei()"));
    assert!(finalize.contains("let priority_economics = Self::evaluated_terminal"));
    assert!(finalize.contains("priority_economics,"));
    assert!(finalize.contains("decision.admitted().then_some(true)"));
    assert!(!finalize.contains("if !decision.admitted()"));
    assert!(source.contains("pub const fn priority_economics(&self) -> &PriorityEconomicsV2"));
    let bridge = include_str!("../src/tx_authority/bridge.rs");
    assert!(bridge.contains("if !candidate.detail.economics().admitted()"));
}

#[test]
fn canonical_dual_fee_evidence_is_consuming_and_raw_free() {
    let tx = TxEip1559 {
        chain_id: 8_453,
        nonce: 7,
        gas_limit: 3_000_000,
        max_fee_per_gas: 200,
        max_priority_fee_per_gas: 100,
        to: TxKind::Call(Address::repeat_byte(0x44)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::from(vec![0x55; 256]),
    };
    let mut l1 = L1BlockInfo {
        l1_base_fee: U256::from(1_055_991_687u64),
        l1_base_fee_scalar: U256::from(5_227u64),
        l1_blob_base_fee_scalar: Some(U256::from(1_014_213u64)),
        l1_blob_base_fee: Some(U256::from(1u64)),
        ..Default::default()
    };
    let evidence =
        CanonicalEnvelopeFactory::calculate_l1_evidence(&tx, &mut l1, BaseSpecId::default())
            .expect("canonical evidence");
    assert_eq!(evidence.fee(), evidence.dummy().fee().max(evidence.surrogate().fee()));
    assert_ne!(evidence.dummy().digest(), evidence.surrogate().digest());
    let source = include_str!("../src/canonical_envelope.rs");
    assert!(!source.contains("pub fn raw_envelope"));
    assert!(!source.contains("impl AsRef"));
    assert!(!source.contains("Serialize"));
}

#[test]
fn fastlz_exact_match_formula_covers_short_and_extension_boundaries() {
    let expected = [(3, 2), (8, 2), (9, 3), (262, 3), (263, 3), (264, 3), (265, 5)];
    for (matched, encoded) in expected {
        let actual = CanonicalEnvelopeFactory::exact_match_encoded_len(matched);
        assert_eq!(actual, encoded, "match {matched}");
        assert!(actual <= matched - 1);
        assert!(actual + 1 <= matched);
    }
}

#[test]
fn fastlz_relaxed_partition_proof_covers_empty_match_only_and_fragmented_literals() {
    assert_eq!(CanonicalEnvelopeFactory::relaxed_legal_partition_upper_bound(0), 0);
    for length in 1..=300 {
        let relaxed = CanonicalEnvelopeFactory::relaxed_legal_partition_upper_bound(length);
        assert!(relaxed <= length + length.div_ceil(32), "length {length}: {relaxed}");
    }
    assert_eq!(CanonicalEnvelopeFactory::exact_match_encoded_len(3), 2);
    assert_eq!(CanonicalEnvelopeFactory::literal_encoded_len(1) * 2 + 2, 6);
}

#[test]
fn fastlz_literal_and_maximum_boundaries_are_exact() {
    let cases = [(0, 0), (1, 2), (31, 32), (32, 33), (33, 35), (261, 270), (262, 271), (263, 272)];
    for (length, expected) in cases {
        assert_eq!(CanonicalEnvelopeFactory::literal_encoded_len(length), expected);
    }
    assert_eq!(
        CanonicalEnvelopeFactory::literal_encoded_len(MAX_CANONICAL_ENVELOPE_LEN),
        MAX_CANONICAL_ENVELOPE_LEN + MAX_CANONICAL_ENVELOPE_LEN.div_ceil(32)
    );
    fn inventory(file: &syn::File, type_name: &str) -> BTreeSet<String> {
        file.items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Impl(item) => match item.self_ty.as_ref() {
                    syn::Type::Path(path)
                        if path
                            .path
                            .segments
                            .last()
                            .is_some_and(|segment| segment.ident == type_name) =>
                    {
                        Some(item)
                    }
                    _ => None,
                },
                _ => None,
            })
            .flat_map(|item| item.items.iter())
            .filter_map(|item| match item {
                syn::ImplItem::Fn(method) if matches!(method.vis, syn::Visibility::Public(_)) => {
                    Some(method.sig.ident.to_string())
                }
                _ => None,
            })
            .collect()
    }
    let file = syn::parse_file(include_str!("../src/tx_authority.rs")).expect("authority syntax");
    let expected = BTreeSet::from(
        [
            "access_digest",
            "beryl_env",
            "deployment_witness",
            "executor",
            "frame",
            "frame_digest",
            "freshness_witness",
            "header_coinbase",
            "header_identity_digest",
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
            "kickback_recipient",
        ]
        .map(str::to_owned),
    );
    assert_eq!(inventory(&file, "CheckedBindingsView"), expected);
    assert_eq!(inventory(&file, "CheckedBerylEnvInputs").len(), 7);
    assert_eq!(inventory(&file, "DeploymentWitness").len(), 3);
    assert_eq!(inventory(&file, "NonceWitness").len(), 5);
    assert_eq!(inventory(&file, "FreshnessWitness").len(), 4);
}
