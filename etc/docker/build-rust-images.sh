#!/usr/bin/env bash
set -euo pipefail

group="${1:-client}"
profile="${2:-release}"

case "$(uname -m)" in
  x86_64)
    platform_pair="linux-amd64"
    ;;
  arm64|aarch64)
    platform_pair="linux-arm64"
    ;;
  *)
    echo "unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

PROFILE="${profile}" PLATFORM_PAIR="${platform_pair}" docker buildx bake -f etc/docker/docker-bake.hcl "${group}" --load
