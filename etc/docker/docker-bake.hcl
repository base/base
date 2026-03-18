variable "PROFILE" {
  default = "release"
}

variable "RUST_VERSION" {
  default = "1.93"
}

variable "REGISTRY_IMAGE" {
  default = "ghcr.io/base/node-reth-dev"
}

variable "PLATFORM_PAIR" {
  default = "linux-amd64"
}

group "default" {
  targets = ["client"]
}

group "rust-services" {
  targets = [
    "client",
    "builder",
    "consensus",
    "proposer",
    "websocket-proxy",
    "ingress-rpc",
    "audit-archiver",
    "batcher",
  ]
}

group "devnet" {
  targets = ["builder", "consensus", "client", "batcher"]
}

group "ingress" {
  targets = ["builder", "consensus", "client", "ingress-rpc", "audit-archiver", "batcher"]
}

target "_rust-service-common" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.rust-services"
  args = {
    PROFILE = "${PROFILE}"
    RUST_VERSION = "${RUST_VERSION}"
  }
  cache-from = ["type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}"]
}

target "client" {
  inherits = ["_rust-service-common"]
  target = "client"
  tags = ["base-reth-node:local"]
}

target "builder" {
  inherits = ["_rust-service-common"]
  target = "builder"
  tags = ["base-builder:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-builder-${PLATFORM_PAIR}",
  ]
}

target "consensus" {
  inherits = ["_rust-service-common"]
  target = "consensus"
  tags = ["base-consensus:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-consensus-${PLATFORM_PAIR}",
  ]
}

target "proposer" {
  inherits = ["_rust-service-common"]
  target = "proposer"
  tags = ["base-proposer:local"]
}

target "websocket-proxy" {
  inherits = ["_rust-service-common"]
  target = "websocket-proxy"
  tags = ["websocket-proxy:local"]
}

target "ingress-rpc" {
  inherits = ["_rust-service-common"]
  target = "ingress-rpc"
  tags = ["ingress-rpc:local"]
}

target "audit-archiver" {
  inherits = ["_rust-service-common"]
  target = "audit-archiver"
  tags = ["audit-archiver:local"]
}

target "batcher" {
  inherits = ["_rust-service-common"]
  target = "batcher"
  tags = ["base-batcher:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-batcher-${PLATFORM_PAIR}",
  ]
}
