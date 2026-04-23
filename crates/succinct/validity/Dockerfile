# Build stage
FROM rust:1.85 AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    libclang-dev \
    pkg-config \
    libssl-dev \
    git \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Install Go 1.24 (required for sp1-recursion-gnark-ffi)
RUN curl -LO https://go.dev/dl/go1.24.0.linux-amd64.tar.gz && \
    tar -C /usr/local -xzf go1.24.0.linux-amd64.tar.gz && \
    rm go1.24.0.linux-amd64.tar.gz
ENV PATH="/usr/local/go/bin:${PATH}"


WORKDIR /build

# Install SP1
RUN curl -L https://sp1.succinct.xyz | bash && \
    ~/.sp1/bin/sp1up -v 6.1.0 && \
    ~/.sp1/bin/cargo-prove prove --version

# Copy only what's needed for the build.
#
# NOTE: SP1 ELFs are not tracked in git; run `just succinct ensure-elfs` on the
# host before `docker build` so crates/succinct/elf/* is populated in the build
# context. The RUN below fails fast with a clear message otherwise.
COPY . .

RUN test -s crates/succinct/elf/range-elf-embedded \
         -a -s crates/succinct/elf/aggregation-elf \
    || (echo "error: SP1 ELFs missing from build context." \
           "Run 'just succinct ensure-elfs' on the host before docker build." \
        >&2 && exit 1)

# Build the server
RUN --mount=type=ssh \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --bin validity --release && \
    cp target/release/validity /build/validity-proposer

# Final stage
FROM rust:1.85-slim

WORKDIR /app

# Install required runtime dependencies
RUN apt-get update && apt-get install -y \
    curl \
    clang \
    pkg-config \
    libssl-dev \
    ca-certificates \
    git \
    libclang-dev \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Install SP1
RUN curl -L https://sp1.succinct.xyz | bash && \
    ~/.sp1/bin/sp1up -v 6.1.0 && \
    ~/.sp1/bin/cargo-prove prove --version

# Copy only the built binaries from builder
COPY --from=builder /build/validity-proposer /usr/local/bin/validity-proposer

# Run the server from its permanent location
CMD ["/usr/local/bin/validity-proposer"]
