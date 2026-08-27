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
    "websocket-proxy",
    "ingress-rpc",
    "audit-archiver",
    "batcher",
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

target "_rust-service-common" {
  context = "."
  dockerfile = "etc/docker/Dockerfile.rust-services"
  args = {
    PROFILE = "${PROFILE}"
    RUST_VERSION = "${RUST_VERSION}"
  }
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
}

target "consensus" {
  inherits = ["_rust-service-common"]
  target = "consensus"
  tags = ["base-consensus:local"]
}

target "builder" {
  inherits = ["_rust-service-common"]
  target = "builder"
  tags = ["base-builder:local"]
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
}

target "sidecrush" {
  inherits = ["_rust-service-common"]
  target = "sidecrush"
  tags = ["sidecrush:local"]
}

target "prover-service" {
  inherits = ["_rust-service-common"]
  target = "prover-service"
  tags = ["base-prover-service:local"]
}

target "zk-host" {
  inherits = ["_rust-service-common"]
  target = "zk-host"
  args = {
    PROFILE                   = "${ZK_HOST_PROFILE}"
    BASE_SUCCINCT_ELF_REQUIRE = "${BASE_SUCCINCT_ELF_REQUIRE}"
  }
  tags = ["base-prover-zk-host:local"]
}
