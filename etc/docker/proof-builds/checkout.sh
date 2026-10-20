#!/bin/sh
set -eu

: "${1:?usage: checkout.sh DIRECTORY REPOSITORY COMMIT}"
: "${2:?repository required}"
: "${3:?commit required}"
case "$3" in
    *[!0-9a-f]*|'') echo "expected a full lowercase commit SHA" >&2; exit 1 ;;
esac
[ "${#3}" -eq 40 ] || { echo "expected a full commit SHA" >&2; exit 1; }
mkdir -p "$1"
cd "$1"
git init
git remote add origin "$2"
git fetch --depth=1 origin "$3"
git reset --hard FETCH_HEAD
[ "$(git rev-parse HEAD)" = "$3" ] || { echo "fetched commit differs from requested SHA" >&2; exit 1; }
