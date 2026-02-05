.PHONY: all build check test lint fmt fmt-check clean doc help build-go-server test-integration

# Default target
all: fmt-check lint build test

# Build all targets
build:
	cargo build --all-targets

# Build in release mode
release:
	cargo build --release --all-targets

# Run cargo check (faster than build)
check:
	cargo check --all-targets

# Run all tests
test:
	cargo test --all-targets

# Build the Go enclave server
build-go-server:
	cd ../op-enclave/op-enclave && go build -o bin/enclave ./cmd/enclave

# Run integration tests (requires Go server to be running)
# Start Go server first: OP_ENCLAVE_SIGNER_KEY=ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 ../op-enclave/bin/enclave
test-integration: build-go-server
	cargo test --package op-enclave-server --test go_interop -- --ignored --test-threads=1

# Run tests with output
test-verbose:
	cargo test --all-targets -- --nocapture

# Run clippy linter
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Format code
fmt:
	cargo fmt --all

# Check formatting without modifying
fmt-check:
	cargo fmt --all -- --check

# Generate documentation
doc:
	cargo doc --no-deps --open

# Clean build artifacts
clean:
	cargo clean

# Run all CI checks (same as GitHub Actions)
ci: fmt-check lint build test

# Watch for changes and run tests (requires cargo-watch)
watch:
	cargo watch -x test

# Update dependencies
update:
	cargo update

# Show help
help:
	@echo "Available targets:"
	@echo "  all              - Run fmt-check, lint, build, and test (default)"
	@echo "  build            - Build all targets in debug mode"
	@echo "  release          - Build all targets in release mode"
	@echo "  check            - Run cargo check (faster than build)"
	@echo "  test             - Run all tests"
	@echo "  test-verbose     - Run tests with output"
	@echo "  test-integration - Run Go interop tests (builds Go server first)"
	@echo "  build-go-server  - Build the Go enclave server"
	@echo "  lint             - Run clippy linter"
	@echo "  fmt              - Format code"
	@echo "  fmt-check        - Check formatting without modifying"
	@echo "  doc              - Generate and open documentation"
	@echo "  clean            - Clean build artifacts"
	@echo "  ci               - Run all CI checks"
	@echo "  watch            - Watch for changes and run tests"
	@echo "  update           - Update dependencies"
	@echo "  help             - Show this help message"
