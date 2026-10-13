#!/bin/sh
export SOURCE_DATE_EPOCH=0
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=yes --remap-path-prefix=${NITRO_SOURCE_ROOT:-/app}=/src --remap-path-prefix=${CARGO_HOME:-/usr/local/cargo}=/cargo"
export CARGO_INCREMENTAL=0
export RUSTC_WRAPPER=
export RUSTC_WORKSPACE_WRAPPER=
export CFLAGS="-ffile-prefix-map=${NITRO_SOURCE_ROOT:-/app}=/src -ffile-prefix-map=${CARGO_HOME:-/usr/local/cargo}=/cargo"
export CXXFLAGS="$CFLAGS"
export OPENSSL_STATIC=1
export OPENSSL_LIB_DIR=/usr/lib
export OPENSSL_INCLUDE_DIR=/usr/include
