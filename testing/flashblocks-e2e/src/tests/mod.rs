//! Test definitions for flashblocks e2e testing.

mod blocks;
mod call;
mod contracts;
mod logs;
mod metering;
mod receipts;
mod subscriptions;

use std::{future::Future, pin::Pin};

use eyre::Result;

use crate::TestClient;

/// A test function that takes a client and returns a result.
pub type TestFn =
    Box<dyn Fn(&TestClient) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> + Send + Sync>;

/// A skip condition function that returns Some(reason) if the test should be skipped.
pub type SkipFn = Box<
    dyn Fn(&TestClient) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> + Send + Sync,
>;

/// A single test case.
pub struct Test {
    /// Test name (used for filtering).
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// The test function to run.
    pub run: TestFn,
    /// Optional skip condition.
    pub skip_if: Option<SkipFn>,
}

impl std::fmt::Debug for Test {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Test")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// A category of related tests.
#[derive(Debug)]
pub struct TestCategory {
    /// Category name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Tests in this category.
    pub tests: Vec<Test>,
}

/// The complete test suite.
#[derive(Debug)]
pub struct TestSuite {
    /// Categories in this test suite.
    pub categories: Vec<TestCategory>,
}

/// Helper macro for creating test functions.
#[macro_export]
macro_rules! test_fn {
    ($f:expr) => {
        Box::new(|client: &$crate::TestClient| Box::pin($f(client)))
    };
}

/// Helper macro for creating skip functions.
#[macro_export]
macro_rules! skip_fn {
    ($f:expr) => {
        Some(Box::new(
            |client: &$crate::TestClient| -> ::std::pin::Pin<
                Box<dyn ::std::future::Future<Output = Option<String>> + Send + '_>,
            > { Box::pin($f(client)) },
        ) as $crate::tests::SkipFn)
    };
}

// ============================================================================
// Shared skip condition helpers
// ============================================================================

/// Check if we have a signer configured.
///
/// Returns Some(reason) if no signer is available, indicating the test should be skipped.
pub fn skip_if_no_signer(client: &TestClient) -> Option<String> {
    if !client.has_signer() { Some("No PRIVATE_KEY configured".to_string()) } else { None }
}

/// Check if we have both a signer and recipient configured.
///
/// Returns Some(reason) if either is missing, indicating the test should be skipped.
pub fn skip_if_no_signer_or_recipient(client: &TestClient) -> Option<String> {
    if !client.has_signer() {
        Some("No PRIVATE_KEY configured".to_string())
    } else if client.recipient().is_none() {
        Some("No --recipient configured".to_string())
    } else {
        None
    }
}

/// Check if we have at least one address configured (signer or recipient).
///
/// Returns Some(reason) if neither is available, indicating the test should be skipped.
pub fn skip_if_no_addresses(client: &TestClient) -> Option<String> {
    if client.signer_address().is_none() && client.recipient().is_none() {
        Some("No PRIVATE_KEY or --recipient configured".to_string())
    } else {
        None
    }
}

/// Build the complete test suite.
pub fn build_test_suite() -> TestSuite {
    TestSuite {
        categories: vec![
            blocks::category(),
            call::category(),
            receipts::category(),
            logs::category(),
            subscriptions::category(),
            metering::category(),
            contracts::category(),
        ],
    }
}
