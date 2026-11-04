#!/bin/bash -eu
# Builds the fuzz targets in fuzz/ (cargo-fuzz) inside ClusterFuzzLite's
# image and copies them to $OUT, each with its seeds from fuzz/seeds/ as
# <target>_seed_corpus.zip. Every target listed in fuzz/Cargo.toml is
# picked up: adding one needs no change here.

cd "$SRC/dbine"
cargo fuzz build -O --debug-assertions

release=fuzz/target/x86_64-unknown-linux-gnu/release
for target in $(cargo fuzz list); do
  cp "$release/$target" "$OUT/"
  if [ -d "fuzz/seeds/$target" ]; then
    (cd "fuzz/seeds/$target" && zip -q -r "$OUT/${target}_seed_corpus.zip" .)
  fi
done
