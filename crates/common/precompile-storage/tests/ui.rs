//! Compile-fail (trybuild) tests for the `#[namespace]` macro error paths.
//!
//! Each fixture in `tests/ui/` exercises a validation that the macro enforces at compile time.
//! The corresponding `.stderr` files capture the exact diagnostic the compiler must emit.
#[test]
fn namespace_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
