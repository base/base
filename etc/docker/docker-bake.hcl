variable "PROFILE" {
  default = "release"
}

variable "RUST_VERSION" {
  default = "1.96.0"
}

variable "BASE_SUCCINCT_ELF_REQUIRE" {
  default = "1"
}

variable "ZK_HOST_PROFILE" {
  default = "release"
}

# The devnet always builds both batchers: the canonical Go op-batcher and the
# Rust base batcher (part of the `base` image), which runs in shadow mode.
variable "DEVNET_TARGETS" {
  default = ["base", "op-batcher"]
}

variable "INGRESS_TARGETS" {
  default = ["base", "ingress-rpc", "audit-archiver", "op-batcher"]
}

group "default" {
  targets = ["base"]
}

group "rust-services" {
  targets = [
    "base",
    "execution",
    "consensus",
    "builder",
    "basectl",
    "snapshotter",
    "proposer",
    "websocket-proxy",
    "ingress-rpc",
    "audit-archiver",
    "sidecrush",
    "prover-service",
    "zk-host",
  ]
}

group "devnet" {
  targets = DEVNET_TARGETS
}

group "ingress" {
  targets = INGRESS_TARGETS
}

target "profiling-tools" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.profiling-tools"
  tags = ["base-profiling-tools:local"]
}

target "_rust-service-common" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.rust-services"
  args = {
    PROFILE = "${PROFILE}"
    RUST_VERSION = "${RUST_VERSION}"
    RUSTFLAGS = PROFILE == "profiling" ? "-C link-arg=-fuse-ld=lld -Cforce-frame-pointers=yes" : "-C link-arg=-fuse-ld=lld"
  }
}

# Keep SCCACHE_CACHE_ID stable for a target so repeated builds reuse cached Rust
# compiler outputs. Use a different ID when targets normally run concurrently,
# otherwise BuildKit's locked sccache mount will serialize those builds.

target "base" {
  inherits = ["_rust-service-common"]
  target = "base"
  args = {
    CARGO_CHEF_ARGS = "--package base --package base-reth-node --package base-consensus --package base-snapshotter-bin"
    CARGO_FEATURES = PROFILE == "profiling" ? "--features=base/jemalloc-prof,base-reth-node/jemalloc-prof" : ""
    SCCACHE_CACHE_ID = "rust-services-base-sccache"
  }
  tags = ["base:local"]
}

target "execution" {
  inherits = ["_rust-service-common"]
  target = "execution"
  args = {
    CARGO_CHEF_ARGS = "--package base-reth-node"
    CARGO_FEATURES = PROFILE == "profiling" ? "--features=jemalloc-prof" : ""
    SCCACHE_CACHE_ID = "rust-services-execution-sccache"
  }
  tags = ["base-execution:local"]
}

target "consensus" {
  inherits = ["_rust-service-common"]
  target = "consensus"
  args = {
    CARGO_CHEF_ARGS = "--package base-consensus"
    SCCACHE_CACHE_ID = "rust-services-consensus-sccache"
  }
  tags = ["base-consensus:local"]
}

target "builder" {
  inherits = ["_rust-service-common"]
  target = "builder"
  args = {
    CARGO_CHEF_ARGS = "--package base-builder-bin"
    CARGO_FEATURES = PROFILE == "profiling" ? "--features=jemalloc-prof" : ""
    SCCACHE_CACHE_ID = "rust-services-builder-sccache"
  }
  tags = ["base-builder:local"]
}

target "basectl" {
  inherits = ["_rust-service-common"]
  target = "basectl"
  args = {
    CARGO_CHEF_ARGS = "--package basectl"
    SCCACHE_CACHE_ID = "rust-services-basectl-sccache"
  }
  tags = ["base-basectl:local"]
}

target "snapshotter" {
  inherits = ["_rust-service-common"]
  target = "snapshotter"
  args = {
    CARGO_CHEF_ARGS = "--package base-snapshotter-bin"
    SCCACHE_CACHE_ID = "rust-services-snapshotter-sccache"
  }
  tags = ["base-snapshotter:local"]
}

target "proposer" {
  inherits = ["_rust-service-common"]
  target = "proposer"
  args = {
    CARGO_CHEF_ARGS = "--package base-proposer-bin"
    SCCACHE_CACHE_ID = "rust-services-proposer-sccache"
  }
  tags = ["base-proposer:local"]
}

target "websocket-proxy" {
  inherits = ["_rust-service-common"]
  target = "websocket-proxy"
  args = {
    CARGO_CHEF_ARGS = "--package websocket-proxy-bin"
    SCCACHE_CACHE_ID = "rust-services-websocket-proxy-sccache"
  }
  tags = ["websocket-proxy:local"]
}

target "ingress-rpc" {
  inherits = ["_rust-service-common"]
  target = "ingress-rpc"
  args = {
    CARGO_CHEF_ARGS = "--package ingress-rpc"
    SCCACHE_CACHE_ID = "rust-services-ingress-rpc-sccache"
  }
  tags = ["ingress-rpc:local"]
}

target "audit-archiver" {
  inherits = ["_rust-service-common"]
  target = "audit-archiver"
  args = {
    CARGO_CHEF_ARGS = "--package audit-archiver"
    SCCACHE_CACHE_ID = "rust-services-audit-archiver-sccache"
  }
  tags = ["audit-archiver:local"]
}

target "op-batcher" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.op-batcher"
  tags = ["op-batcher:local"]
}

target "sidecrush" {
  inherits = ["_rust-service-common"]
  target = "sidecrush"
  args = {
    CARGO_CHEF_ARGS = "--package base-sidecrush-bin"
    SCCACHE_CACHE_ID = "rust-services-sidecrush-sccache"
  }
  tags = ["sidecrush:local"]
}

target "prover-service" {
  inherits = ["_rust-service-common"]
  target = "prover-service"
  args = {
    CARGO_CHEF_ARGS = "--package base-prover-service-bin"
    SCCACHE_CACHE_ID = "rust-services-prover-service-sccache"
  }
  tags = ["base-prover-service:local"]
}

target "nitro-host-local" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.nitro-host"
  args = {
    PROFILE        = "${PROFILE}"
    CARGO_FEATURES = "--features local"
  }
  tags = ["base-prover-nitro-host:local"]
}

target "zk-host" {
  inherits = ["_rust-service-common"]
  target = "zk-host"
  args = {
    PROFILE                   = "${ZK_HOST_PROFILE}"
    BASE_SUCCINCT_ELF_REQUIRE = "${BASE_SUCCINCT_ELF_REQUIRE}"
    SCCACHE_CACHE_ID          = "rust-services-zk-host-sccache"
  }
  tags = ["base-prover-zk-host:local"]
}
