# Vibenet setup image: foundry (forge + cast) + yq + jq + bash.
# Pre-builds the demo contracts so container start only needs to broadcast.
#
# The foundry:stable image is debian-based, so we use apt for extras.

FROM ghcr.io/foundry-rs/foundry:stable AS builder

USER root
WORKDIR /setup

COPY etc/vibenet/setup/contracts/foundry.toml /setup/contracts/foundry.toml
COPY etc/vibenet/setup/contracts/src/ /setup/contracts/src/

RUN cd /setup/contracts && forge build --silent

FROM ghcr.io/foundry-rs/foundry:stable

USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends bash bc curl ca-certificates jq \
 && rm -rf /var/lib/apt/lists/*
# yq has a static binary per-arch; pick the one that matches this container.
RUN set -eux; \
    case "$(uname -m)" in \
      x86_64) YQ_ARCH=amd64 ;; \
      aarch64|arm64) YQ_ARCH=arm64 ;; \
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/yq \
      "https://github.com/mikefarah/yq/releases/download/v4.44.3/yq_linux_${YQ_ARCH}"; \
    chmod +x /usr/local/bin/yq

WORKDIR /setup

# Bring in the already-built artifacts + sources (forge create needs both).
COPY --from=builder /setup/contracts /setup/contracts

COPY etc/vibenet/setup/contracts.yaml /setup/contracts.yaml
COPY etc/vibenet/setup/deploy-contracts.sh /usr/local/bin/deploy-contracts.sh
RUN chmod +x /usr/local/bin/deploy-contracts.sh

ENTRYPOINT ["/usr/local/bin/deploy-contracts.sh"]
