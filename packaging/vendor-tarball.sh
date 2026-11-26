#!/bin/sh
# Writes the two source tarballs the COPR and PPA builds need, both offline:
#   dist/src/blessh-<version>.tar.gz          git archive of HEAD
#   dist/src/blessh-<version>-vendor.tar.xz   `cargo vendor` of Cargo.lock
# Usage: packaging/vendor-tarball.sh
set -eu
cd "$(dirname "$0")/.."
VER=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
OUT=dist/src
mkdir -p "$OUT"
git archive --format=tar.gz --prefix="blessh-$VER/" -o "$OUT/blessh-$VER.tar.gz" HEAD
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
cargo vendor --locked --versioned-dirs "$TMP/vendor" > /dev/null
tar -C "$TMP" -cJf "$OUT/blessh-$VER-vendor.tar.xz" vendor
ls -l "$OUT"
