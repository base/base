#!/usr/bin/env bash
# THROWAWAY one-off measurement kit — P2P block-latency measurement.
#
# Pulls each region's append-only CSV to ./collected/<region>.csv via rsync
# (falls back to scp). Safe to run repeatedly (partial pulls at the T0+24h
# checkpoint and the final pull at T0+7d) — rsync resumes, and the local files
# are overwritten with the latest full copy each run.
#
# Usage:
#   ./collect_logs.sh [hosts-file] [out-dir]
#     hosts-file  default: ./hosts   (see hosts.example for the format)
#     out-dir     default: ./collected
#
# Hosts file format (whitespace-separated, '#' comments ignored):
#   <region>  <[user@]host>  <remote-csv-path>
#
# --- S3 one-shot alternative -------------------------------------------------
# If the observers write their CSV to S3 instead of local disk (or you sync it
# there), skip the ssh path entirely and pull with a single command per region:
#
#   for r in us-east us-west eu-central eu-north ap-northeast ap-southeast; do
#     aws s3 cp "s3://YOUR_BUCKET/latency/${r}.csv" "./collected/${r}.csv"
#   done
#
# (Or `aws s3 sync s3://YOUR_BUCKET/latency/ ./collected/` for all at once.)
# -----------------------------------------------------------------------------
set -euo pipefail

HOSTS_FILE="${1:-./hosts}"
OUT_DIR="${2:-./collected}"

if [[ ! -f "${HOSTS_FILE}" ]]; then
  echo "ERROR: hosts file '${HOSTS_FILE}' not found. Copy hosts.example to hosts." >&2
  exit 1
fi

mkdir -p "${OUT_DIR}"

have_rsync=0
if command -v rsync >/dev/null 2>&1; then
  have_rsync=1
fi

fail=0
while read -r region host remote_path _rest; do
  # Skip comments and blank lines.
  [[ -z "${region:-}" || "${region:0:1}" == "#" ]] && continue

  if [[ -z "${host:-}" || -z "${remote_path:-}" ]]; then
    echo "WARN: malformed line for region '${region}', skipping." >&2
    fail=1
    continue
  fi

  dest="${OUT_DIR}/${region}.csv"
  echo "==> ${region}: ${host}:${remote_path} -> ${dest}"

  if [[ "${have_rsync}" -eq 1 ]]; then
    if ! rsync -az --partial "${host}:${remote_path}" "${dest}"; then
      echo "WARN: rsync failed for ${region}." >&2
      fail=1
    fi
  else
    if ! scp "${host}:${remote_path}" "${dest}"; then
      echo "WARN: scp failed for ${region}." >&2
      fail=1
    fi
  fi
done < "${HOSTS_FILE}"

echo
echo "==> Collected files:"
ls -la "${OUT_DIR}" || true

if [[ "${fail}" -ne 0 ]]; then
  echo "One or more regions failed to collect — see WARN lines above." >&2
  exit 1
fi
