variable "PROFILE" {
  default = "release"
}

variable "RUST_VERSION" {
  default = "1.95.0"
}

variable "BASE_SUCCINCT_ELF_REQUIRE" {
  default = "1"
}

variable "ZK_HOST_PROFILE" {
  default = "release"
}

variable "REGISTRY_IMAGE" {
  default = "ghcr.io/base/node-reth-dev"
}

variable "PLATFORM_PAIR" {
  default = "linux-amd64"
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
    "execution",
    "consensus",
    "builder",
    "basectl",
    "snapshotter",
    "proposer",
    "challenger",
    "websocket-proxy",
    "ingress-rpc",
    "audit-archiver",
    "batcher",
    "sidecrush",
    "prover-service",
    "zk-host",
    "da-server",
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
  cache-from = ["type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}"]
}

target "base" {
  inherits = ["_rust-service-common"]
  target = "base"
  tags = ["base:local"]
}

target "execution" {
  inherits = ["_rust-service-common"]
  target = "execution"
  tags = ["base-execution:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-execution-${PLATFORM_PAIR}",
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

target "builder" {
  inherits = ["_rust-service-common"]
  target = "builder"
  tags = ["base-builder:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-builder-${PLATFORM_PAIR}",
  ]
}

target "basectl" {
  inherits = ["_rust-service-common"]
  target = "basectl"
  tags = ["base-basectl:local"]
}

target "snapshotter" {
  inherits = ["_rust-service-common"]
  target = "snapshotter"
  tags = ["base-snapshotter:local"]
}

target "proposer" {
  inherits = ["_rust-service-common"]
  target = "proposer"
  tags = ["base-proposer:local"]
}

target "challenger" {
  inherits = ["_rust-service-common"]
  target = "challenger"
  tags = ["base-challenger:local"]
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

target "da-server" {
  inherits = ["_rust-service-common"]
  target = "da-server"
  tags = ["base-da-server:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-da-server-${PLATFORM_PAIR}",
  ]
}

target "sidecrush" {
  inherits = ["_rust-service-common"]
  target = "sidecrush"
  tags = ["sidecrush:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-sidecrush-${PLATFORM_PAIR}",
  ]
}

target "prover-service" {
  inherits = ["_rust-service-common"]
  target = "prover-service"
  tags = ["base-prover-service:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-prover-service-${PLATFORM_PAIR}",
  ]
}

target "zk-host" {
  inherits = ["_rust-service-common"]
  target = "zk-host"
  args = {
    PROFILE                   = "${ZK_HOST_PROFILE}"
    BASE_SUCCINCT_ELF_REQUIRE = "${BASE_SUCCINCT_ELF_REQUIRE}"
  }
  tags = ["base-prover-zk-host:local"]
  cache-from = [
    "type=registry,ref=${REGISTRY_IMAGE}:cache-${PLATFORM_PAIR}",
    "type=registry,ref=${REGISTRY_IMAGE}:cache-zk-host-${PLATFORM_PAIR}",
  ]
}
