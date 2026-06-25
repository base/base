variable "PROFILE" {
  default = "release"
}

variable "ZK_PROVER_PROFILE" {
  default = "release"
}

variable "RUST_VERSION" {
  default = "1.94.1"
}

variable "BASE_SUCCINCT_ELF_REQUIRE" {
  default = "1"
}

variable "REGISTRY_IMAGE" {
  default = "ghcr.io/base/node-reth-dev"
}

variable "PLATFORM_PAIR" {
  default = "linux-amd64"
}

group "default" {
  targets = ["base"]
}

group "rust-services" {
  targets = [
    "base",
    "proposer",
    "websocket-proxy",
    "ingress-rpc",
    "audit-archiver",
    "batcher",
    "zk-prover",
  ]
}

group "devnet" {
  targets = ["base", "batcher", "zk-prover"]
}

group "ingress" {
  targets = [
    "base",
    "ingress-rpc",
    "audit-archiver",
    "batcher",
  ]
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

target "base" {
  inherits = ["_rust-service-common"]
  target = "base"
  tags = ["base:local"]
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

target "zk-prover" {
  inherits = ["_rust-service-common"]
  target = "zk-prover"
  args = {
    PROFILE                   = "${ZK_PROVER_PROFILE}"
    BASE_SUCCINCT_ELF_REQUIRE = "${BASE_SUCCINCT_ELF_REQUIRE}"
  }
  tags = ["base-prover-zk:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-zk-prover-${PLATFORM_PAIR}",
  ]
}
