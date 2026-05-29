#!/usr/bin/env bash
# Render a Docker Compose override that mounts an extra CA certificate.

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: ca-compose.sh CA_CERT_FILE OUTPUT \
  --service-name SERVICE \
  --service-command COMMAND \
  --system-ca-file PATH \
  --extra-ca-file PATH \
  --combined-ca-file PATH
EOF
}

yaml_quote() {
  local value="$1"
  local quote="'"

  printf "'%s'" "${value//$quote/$quote$quote}"
}

shell_quote() {
  local value="$1"
  local quote="'"
  local escaped_quote="'\\''"

  printf "'%s'" "${value//$quote/$escaped_quote}"
}

absolute_path() {
  local path="$1"
  local dir
  local file

  dir="$(dirname "$path")"
  file="$(basename "$path")"
  dir="$(cd "$dir" && pwd -P)"
  printf '%s/%s' "$dir" "$file"
}

if [ "$#" -lt 2 ]; then
  usage
  exit 2
fi

ca_cert_file="$1"
output="$2"
shift 2

service_name=""
service_command=""
system_ca_file=""
extra_ca_file=""
combined_ca_file=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --service-name)
      service_name="${2:?--service-name requires a value}"
      shift 2
      ;;
    --service-command)
      service_command="${2:?--service-command requires a value}"
      shift 2
      ;;
    --system-ca-file)
      system_ca_file="${2:?--system-ca-file requires a value}"
      shift 2
      ;;
    --extra-ca-file)
      extra_ca_file="${2:?--extra-ca-file requires a value}"
      shift 2
      ;;
    --combined-ca-file)
      combined_ca_file="${2:?--combined-ca-file requires a value}"
      shift 2
      ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [ -z "$service_name" ] ||
  [ -z "$service_command" ] ||
  [ -z "$system_ca_file" ] ||
  [ -z "$extra_ca_file" ] ||
  [ -z "$combined_ca_file" ]; then
  usage
  exit 2
fi

if [ ! -f "$ca_cert_file" ]; then
  echo "ERROR: CA certificate file not found: $ca_cert_file" >&2
  exit 1
fi

ca_cert_file="$(absolute_path "$ca_cert_file")"
entrypoint_script="cat $(shell_quote "$system_ca_file") $(shell_quote "$extra_ca_file") > $(shell_quote "$combined_ca_file") && exec $(shell_quote "$service_command") \"\$@\""

cat >"$output" <<EOF
services:
  $(yaml_quote "$service_name"):
    entrypoint:
      - $(yaml_quote "/bin/sh")
      - $(yaml_quote "-c")
      - $(yaml_quote "$entrypoint_script")
      - $(yaml_quote "$service_name")
    environment:
      - $(yaml_quote "SSL_CERT_FILE=$combined_ca_file")
    volumes:
      - type: bind
        source: $(yaml_quote "$ca_cert_file")
        target: $(yaml_quote "$extra_ca_file")
        read_only: true
EOF
