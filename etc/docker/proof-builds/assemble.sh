#!/bin/sh
set -eu

case "${1:?assembly required}" in
    init-ramdisk) linuxkit build --format kernel+initrd --no-sbom --name init-ramdisk ./init-ramdisk.yaml ;;
    user-ramdisk) linuxkit build --format kernel+initrd --no-sbom --name user-ramdisk ./user-ramdisk.yaml ;;
    eif)
        # The timestamp is part of the EIF bytes, not just its filesystem metadata.
        eif_build \
            --kernel bootstrap/bzImage \
            --kernel_config bootstrap/bzImage.config \
            --cmdline "$(cat cmdline)" \
            --ramdisk init-ramdisk-initrd.img \
            --ramdisk user-ramdisk-initrd.img \
            --build-time "1970-01-01T00:00:00+00:00" \
            --output eif.bin ;;
    *) echo "unknown assembly: $1" >&2; exit 1 ;;
esac
