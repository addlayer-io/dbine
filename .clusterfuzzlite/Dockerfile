# ClusterFuzzLite's build image for the fuzz targets in fuzz/ (build.sh):
# OSS-Fuzz's Rust builder, with nightly Rust and cargo-fuzz. Pinned by
# digest; to move it, pull the new image and put its digest here.
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f
COPY . $SRC/dbine
WORKDIR $SRC/dbine
COPY .clusterfuzzlite/build.sh $SRC/
