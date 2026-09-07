variable "PROFILE" {
  default = "release"
}

variable "RUST_VERSION" {
  default = "1.96.0"
}

variable "DEVNET_TARGETS" {
  default = ["base", "batcher"]
}

variable "INGRESS_TARGETS" {
  default = ["base", "batcher", "ingress-rpc", "audit-archiver"]
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
    "ingress-rpc",
    "audit-archiver",
    "batcher",
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

target "_rust-service-common" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.rust-services"
  args = {
    PROFILE = "${PROFILE}"
    RUST_VERSION = "${RUST_VERSION}"
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

target "batcher" {
  inherits = ["_rust-service-common"]
  target = "batcher"
  args = {
    CARGO_CHEF_ARGS = "--package base-batcher-bin"
    SCCACHE_CACHE_ID = "rust-services-batcher-sccache"
  }
  tags = ["base-batcher:local"]
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
