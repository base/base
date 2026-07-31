//! Compile-fail coverage for procedural macro diagnostics.

#[test]
fn rejects_case_normalized_field_name_collisions() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/case_normalized_storable_fields.rs");
    tests.compile_fail("tests/ui/case_normalized_contract_fields.rs");
}
