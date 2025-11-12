# Contributing to Base Reth Node

Thank you for your interest in contributing to Base Reth Node! This document provides guidelines and instructions for contributing to the project.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Setup](#development-setup)
- [Making Contributions](#making-contributions)
- [Testing](#testing)
- [Code Style](#code-style)
- [Pull Request Process](#pull-request-process)
- [Commit Guidelines](#commit-guidelines)

## Code of Conduct

By participating in this project, you agree to maintain a respectful and inclusive environment for all contributors.

## Getting Started

### Prerequisites

Before you begin, ensure you have the following installed:

- **Rust**: Version 1.88 or later (install via [rustup](https://rustup.rs/))
- **Just**: Command runner for development tasks ([installation guide](https://github.com/casey/just#installation))
- **Git**: For version control
- **Build essentials**: On Linux, you may need `libclang-dev`, `pkg-config`, `curl`, and `build-essential`

### Fork and Clone

1. Fork the repository on GitHub
2. Clone your fork locally:
   ```bash
   git clone https://github.com/YOUR_USERNAME/node-reth.git
   cd node-reth
   ```
3. Add the upstream repository:
   ```bash
   git remote add upstream https://github.com/base/node-reth.git
   ```

## Development Setup

### Building the Project

Build the project in release mode:

```bash
just build
```

For maximum performance (with all optimizations):

```bash
just build-maxperf
```

### Running Tests

Run all tests:

```bash
just test
```

Run tests continuously during development:

```bash
just watch-test
```

### Code Quality Checks

Run formatting and linting checks:

```bash
just check
```

This command runs:
- `cargo fmt --all -- --check` (formatting)
- `cargo clippy --all-targets -- -D warnings` (linting)
- `cargo test` (tests)

### Auto-fixing Issues

Automatically fix formatting and clippy warnings:

```bash
just fix
```

## Making Contributions

### Finding Issues

- Check the [issue tracker](https://github.com/base/node-reth/issues) for open issues
- Look for issues labeled `good first issue` or `help wanted`
- Feel free to ask questions in issue comments before starting work

### Types of Contributions

We welcome various types of contributions:

- **Bug fixes**: Fix existing bugs or issues
- **Features**: Implement new functionality
- **Tests**: Add or improve test coverage
- **Documentation**: Improve code comments, README, or guides
- **Performance**: Optimize existing code
- **Code quality**: Refactoring and cleanup

### Before Starting Work

1. **Check for existing work**: Search issues and PRs to avoid duplicate efforts
2. **Discuss major changes**: For significant features or changes, open an issue first to discuss the approach
3. **Create an issue**: If one doesn't exist, create an issue describing what you plan to work on

## Testing

### Unit Tests

Run unit tests for a specific crate:

```bash
cd crates/flashblocks-rpc
cargo test
```

### Integration Tests

Some crates have integration tests that require additional setup:

```bash
# Build the base-reth-node binary first
cargo build --bin base-reth-node

# Run integration tests (from the crate directory)
cd crates/flashblocks-rpc
cargo test --features "integration"
```

### Writing Tests

- Add tests for all new functionality
- Ensure tests are deterministic and don't depend on external state
- Use descriptive test names that explain what is being tested
- Include both positive and negative test cases

Example test structure:

```rust
#[tokio::test]
async fn test_feature_name() -> eyre::Result<()> {
    // Setup
    let node = setup_node().await?;
    
    // Execute
    let result = node.some_operation().await?;
    
    // Assert
    assert_eq!(result.expected_field, expected_value);
    
    Ok(())
}
```

## Code Style

### Rust Style Guidelines

- Follow the official [Rust style guide](https://doc.rust-lang.org/1.0.0/style/)
- Use `cargo fmt` to format your code
- Address all `clippy` warnings
- Use meaningful variable and function names
- Add documentation comments for public APIs

### Documentation

- Add rustdoc comments for all public functions, types, and modules
- Use triple-slash (`///`) for documentation comments
- Include examples in documentation where appropriate

Example:

```rust
/// Meters a transaction bundle and returns gas usage statistics.
///
/// # Arguments
///
/// * `provider` - The blockchain provider instance
/// * `bundle` - The transaction bundle to meter
///
/// # Returns
///
/// Returns a `MeterBundleResponse` containing gas usage and timing information.
///
/// # Errors
///
/// Returns an error if the bundle simulation fails.
pub async fn meter_bundle(
    provider: Provider,
    bundle: Bundle,
) -> eyre::Result<MeterBundleResponse> {
    // Implementation
}
```

## Pull Request Process

### Creating a Pull Request

1. **Keep changes focused**: One PR should address one issue or feature
2. **Update from upstream**: Rebase on the latest `main` branch before submitting
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```
3. **Run checks**: Ensure all tests pass and code is formatted
   ```bash
   just check
   ```
4. **Push to your fork**:
   ```bash
   git push origin your-branch-name
   ```
5. **Create the PR**: Go to GitHub and create a pull request from your fork to the upstream repository

### PR Description

Your PR description should include:

- **Summary**: Brief description of what the PR does
- **Motivation**: Why this change is needed
- **Changes**: List of main changes made
- **Testing**: How the changes were tested
- **Related Issues**: Link to related issues (e.g., "Closes #123")

Example template:

```markdown
## Summary
Adds comprehensive test coverage for transaction property validation.

## Motivation
Resolves TODOs in the test suite to improve test coverage and catch potential regressions.

## Changes
- Added assertions for transaction nonce, gas limit, and recipient
- Added test case for receipt validation across multiple payloads

## Testing
- All existing tests pass
- New assertions verify transaction properties correctly
- Tested receipt cleanup when new payloads are sent

## Related Issues
Closes #123
```

### Review Process

- Be patient and respectful during code review
- Address all review comments
- Push new commits to your branch to update the PR
- Avoid force-pushing after review has started (unless necessary)

## Commit Guidelines

### Commit Messages

Write clear and descriptive commit messages:

- Use the imperative mood ("Add feature" not "Added feature")
- Keep the first line under 72 characters
- Add a blank line between the subject and body
- Use the body to explain what and why, not how

Example:

```
Add comprehensive test coverage for transaction validation

- Implement TODO: verify transaction properties in test_get_transaction_by_hash_pending
- Add assertions for nonce, gas limit, and recipient address
- Implement TODO: test receipt cleanup across multiple payloads
- Verify that old receipts are not returned after new payload

This improves test coverage and ensures transaction properties
are correctly validated in the pending state.
```

### Commit Organization

- Make atomic commits (one logical change per commit)
- Avoid mixing formatting changes with functional changes
- Break large changes into smaller, reviewable commits

## Additional Resources

- [Reth Documentation](https://paradigmxyz.github.io/reth/)
- [Base Documentation](https://docs.base.org/)
- [Rust Book](https://doc.rust-lang.org/book/)
- [Optimism Specs](https://github.com/ethereum-optimism/optimism)

## Questions?

If you have questions or need help:

- Open an issue for discussion
- Check existing issues and PRs for similar questions
- Join the [Base Discord](https://base.org/discord)

Thank you for contributing to Base Reth Node! 🚀
