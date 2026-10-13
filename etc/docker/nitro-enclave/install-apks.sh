#!/bin/sh
set -eu
lock=$(realpath "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
while read -r checksum url; do
    file=${url##*/}
    wget -q -O "$file" "$url"
    printf '%s  %s\n' "$checksum" "$file" | sha256sum -c -
done < "$lock"
# Resolve only against the digest-pinned base and the verified dependency closure.
apk add --no-network --repositories-file /dev/null ./*.apk
