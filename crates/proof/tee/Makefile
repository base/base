.PHONY: all build check test lint fmt fmt-check clean doc help build-go-server test-integration \
        build-eif musl-release verify-pcr0 verify-reproducible clean-eif run-server

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
	cargo test --package enclave-server --test go_interop -- --ignored --test-threads=1

# Run tests with output
test-verbose:
	cargo test --all-targets -- --nocapture

# Run the enclave server locally (HTTP mode on port 1234)
run-server:
	RUST_LOG=info OP_ENCLAVE_SIGNER_KEY=ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
		cargo run --package enclave-server --bin enclave

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
	@echo "  run-server       - Run enclave server locally (HTTP on port 1234)"
	@echo "  build-go-server  - Build the Go enclave server"
	@echo "  lint             - Run clippy linter"
	@echo "  fmt              - Format code"
	@echo "  fmt-check        - Check formatting without modifying"
	@echo "  doc              - Generate and open documentation"
	@echo "  clean            - Clean build artifacts"
	@echo "  ci               - Run all CI checks"
	@echo "  watch            - Watch for changes and run tests"
	@echo "  update           - Update dependencies"
	@echo "  build-eif        - Build EIF using Docker"
	@echo "  musl-release     - Local musl release build"
	@echo "  verify-pcr0      - Show PCR0 for built EIF"
	@echo "  verify-reproducible - Build twice and compare PCR0"
	@echo "  clean-eif        - Clean EIF artifacts"
	@echo "  help             - Show this help message"

# =============================================================================
# EIF Build Targets
# =============================================================================

EIF_IMAGE_NAME := enclave-rust-eif
EIF_OUTPUT := eif.bin

# Build EIF using Docker
build-eif:
	docker build -f Dockerfile.enclave -t $(EIF_IMAGE_NAME) .
	docker create --name eif-extract $(EIF_IMAGE_NAME)
	docker cp eif-extract:/build/eif.bin $(EIF_OUTPUT)
	docker rm eif-extract
	@echo "EIF built successfully: $(EIF_OUTPUT)"
	@ls -lh $(EIF_OUTPUT)

# Local musl release build (requires musl toolchain installed)
musl-release:
	OPENSSL_STATIC=1 cargo build --release --target x86_64-unknown-linux-musl --package enclave-server --bin enclave
	@echo "Binary built at: target/x86_64-unknown-linux-musl/release/enclave"
	@ls -lh target/x86_64-unknown-linux-musl/release/enclave

# Show PCR0 for built EIF (requires nitro-cli)
verify-pcr0:
	@if [ -f $(EIF_OUTPUT) ]; then \
		nitro-cli describe-eif --eif-path $(EIF_OUTPUT) | grep -A1 PCR0; \
	else \
		echo "Error: $(EIF_OUTPUT) not found. Run 'make build-eif' first."; \
		exit 1; \
	fi

# Build twice and compare PCR0 for reproducibility verification
verify-reproducible:
	@echo "Building EIF first time..."
	$(MAKE) build-eif
	@mv $(EIF_OUTPUT) eif-first.bin
	@PCR0_FIRST=$$(nitro-cli describe-eif --eif-path eif-first.bin 2>/dev/null | grep -A1 PCR0 | tail -1 | tr -d ' ",' || echo "nitro-cli not available"); \
	echo "First build PCR0: $$PCR0_FIRST"
	@echo ""
	@echo "Building EIF second time..."
	$(MAKE) build-eif
	@mv $(EIF_OUTPUT) eif-second.bin
	@PCR0_SECOND=$$(nitro-cli describe-eif --eif-path eif-second.bin 2>/dev/null | grep -A1 PCR0 | tail -1 | tr -d ' ",' || echo "nitro-cli not available"); \
	echo "Second build PCR0: $$PCR0_SECOND"
	@echo ""
	@if command -v nitro-cli >/dev/null 2>&1; then \
		PCR0_FIRST=$$(nitro-cli describe-eif --eif-path eif-first.bin | grep -A1 PCR0 | tail -1 | tr -d ' ",'); \
		PCR0_SECOND=$$(nitro-cli describe-eif --eif-path eif-second.bin | grep -A1 PCR0 | tail -1 | tr -d ' ",'); \
		if [ "$$PCR0_FIRST" = "$$PCR0_SECOND" ]; then \
			echo "SUCCESS: PCR0 values match - build is reproducible!"; \
		else \
			echo "FAILURE: PCR0 values differ - build is NOT reproducible"; \
			exit 1; \
		fi; \
	else \
		echo "Comparing file hashes (nitro-cli not available)..."; \
		HASH1=$$(sha256sum eif-first.bin | cut -d' ' -f1); \
		HASH2=$$(sha256sum eif-second.bin | cut -d' ' -f1); \
		echo "First:  $$HASH1"; \
		echo "Second: $$HASH2"; \
		if [ "$$HASH1" = "$$HASH2" ]; then \
			echo "SUCCESS: File hashes match!"; \
		else \
			echo "WARNING: File hashes differ (may still have same PCR0)"; \
		fi; \
	fi
	@rm -f eif-first.bin eif-second.bin

# Clean EIF artifacts
clean-eif:
	rm -f $(EIF_OUTPUT) eif-first.bin eif-second.bin
	docker rmi $(EIF_IMAGE_NAME) 2>/dev/null || true
	@echo "EIF artifacts cleaned"
