# Final stage
FROM ubuntu:22.04

WORKDIR /app

# Install Rust and required dependencies
RUN apt-get update && apt-get install -y \
    curl \
    clang \
    build-essential \
    git \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH=/root/.cargo/bin:$PATH

# Verify Rust installation
RUN rustc --version && cargo --version

# Install SP1
RUN curl -L https://sp1.succinct.xyz | bash && \
    ~/.sp1/bin/sp1up && \
    ~/.sp1/bin/cargo-prove prove --version
    
# Copy the relevant OP Succinct server files.
COPY Cargo.toml Cargo.lock ./
COPY scripts/prove ./scripts/prove
COPY proposer/succinct ./proposer/succinct
COPY scripts/witnessgen ./scripts/witnessgen
COPY elf ./elf
COPY utils ./utils
COPY programs ./programs

# Build the server
RUN cargo build --bin server --release

# Expose port based on environment variable or default to 3000
ENV PORT=${PORT:-3000}
EXPOSE $PORT

# Run the server
CMD ["/app/target/release/server"]
