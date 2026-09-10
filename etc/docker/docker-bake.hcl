variable "PROFILE" {
  default = "release"
}

variable "RUST_VERSION" {
  default = "1.96.0"
}

variable "DEVNET_TARGETS" {
  default = ["base", "op-batcher"]
}

variable "INGRESS_TARGETS" {
  default = ["base", "audit-archiver", "op-batcher"]
}

group "default" {
  targets = ["base"]
}

group "rust-services" {
  targets = [
    "base",
    "basectl",
    "snapshotter",
    "proposer",
    "websocket-proxy",
    "audit-archiver",
    "sidecrush",
    "prover-service",
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
    CARGO_CHEF_ARGS = "--package base --package base-snapshotter-bin"
    CARGO_FEATURES = PROFILE == "profiling" ? "--features=base/jemalloc-prof" : ""
    SCCACHE_CACHE_ID = "rust-services-base-sccache"
  }
  tags = ["base:local"]
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
  tags = ["base-proof-service-server:local"]
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
