#!/bin/sh
set -eu
yaml_dir=$(realpath "$1")
inputs=$(pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for variant in first second; do
    mkdir -p "$work/$variant/bootstrap"
    cp "$inputs/bootstrap/init" "$inputs/bootstrap/nsm.ko" "$work/$variant/bootstrap/"
    cp "$inputs/base-prover-nitro-enclave" "$work/$variant/"
    cd "$work/$variant"
    if [ "$variant" = first ]; then
        touch -t 200101010000 bootstrap/* base-prover-nitro-enclave
        chmod 755 bootstrap/* base-prover-nitro-enclave
    else
        touch -t 202510101010 bootstrap/* base-prover-nitro-enclave
        chmod 600 bootstrap/* base-prover-nitro-enclave
    fi
    linuxkit build --format kernel+initrd --no-sbom --name init-ramdisk "$yaml_dir/init-ramdisk.yaml"
    linuxkit build --format kernel+initrd --no-sbom --name user-ramdisk "$yaml_dir/user-ramdisk.yaml"
done
cmp "$work/first/init-ramdisk-initrd.img" "$work/second/init-ramdisk-initrd.img"
cmp "$work/first/user-ramdisk-initrd.img" "$work/second/user-ramdisk-initrd.img"
echo "Ramdisk bytes match despite different source paths, timestamps and modes"
