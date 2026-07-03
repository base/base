use safe_core_verifier::{
    checks::{Check, convention_x::ConventionXCheck},
    context::FileContext,
    languages::Language,
};

fn parse_code(lang: Language, code: &str) -> anyhow::Result<(tree_sitter::Tree, String)> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.tree_sitter_language())?;
    let tree = parser.parse(code, None).ok_or_else(|| anyhow::anyhow!("Parse falhou"))?;
    Ok((tree, code.to_string()))
}

#[tokio::test]
async fn rust_function_with_external_data_without_x_prefix_fails() {
    let code = r#"
fn process_user(input: String, data: Vec<u8>) -> bool {
    input.len() > 0
}
"#;
    let (tree, code_str) = parse_code(Language::Rust, code).unwrap();
    let ctx = FileContext {
        path: "test.rs".into(),
        language: Language::Rust,
        code: code_str,
        tree,
        content_hash: 0,
    };

    let check = ConventionXCheck;
    let result = check.execute(&ctx).await.unwrap();

    assert!(!result.passed, "Deve falhar: função recebe String/Vec sem x_");
    assert!(!result.issues.is_empty());
    assert!(result.issues[0].message.contains("process_user"));
}

#[tokio::test]
async fn rust_function_with_x_prefix_passes() {
    let code = r#"
fn x_process_user(x_input: String, x_data: Vec<u8>) -> bool {
    x_input.len() > 0
}
"#;
    let (tree, code_str) = parse_code(Language::Rust, code).unwrap();
    let ctx = FileContext {
        path: "test.rs".into(),
        language: Language::Rust,
        code: code_str,
        tree,
        content_hash: 0,
    };

    let check = ConventionXCheck;
    let result = check.execute(&ctx).await.unwrap();

    assert!(result.passed, "Deve passar: função tem x_");
}

#[tokio::test]
async fn python_eval_detected_as_unsafe() {
    let code = r#"
def process(data):
    result = eval(data)
    return result
"#;
    let (tree, code_str) = parse_code(Language::Python, code).unwrap();
    let ctx = FileContext {
        path: "test.py".into(),
        language: Language::Python,
        code: code_str,
        tree,
        content_hash: 0,
    };

    let check = safe_core_verifier::checks::safety::SafetyCheck;
    let result = check.execute(&ctx).await.unwrap();

    assert!(!result.passed);
    assert!(result.issues.iter().any(|i| i.message.contains("eval")));
}

#[tokio::test]
async fn clean_code_passes_all_checks() {
    let code = r#"
fn x_validate_input(x_input: &str) -> Result<usize, ParseError> {
    let len = x_input.parse::<usize>()?;
    if len > 1000 {
        return Err(ParseError::TooLong);
    }
    Ok(len)
}
"#;
    let (tree, code_str) = parse_code(Language::Rust, code).unwrap();
    let ctx = FileContext {
        path: "clean.rs".into(),
        language: Language::Rust,
        code: code_str,
        tree,
        content_hash: 0,
    };

    let checks: Vec<Box<dyn safe_core_verifier::checks::Check>> =
        vec![Box::new(ConventionXCheck), Box::new(safe_core_verifier::checks::safety::SafetyCheck)];

    for check in &checks {
        let result = check.execute(&ctx).await.unwrap();
        assert!(
            result.passed,
            "Check '{}' falhou em código limpo: {:?}",
            check.name(),
            result.issues
        );
    }
}
