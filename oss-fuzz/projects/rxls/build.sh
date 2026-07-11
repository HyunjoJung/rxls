#!/bin/bash -eu
# OSS-Fuzz build script for rxls. Builds the libFuzzer targets and copies them
# (plus a seed corpus) into $OUT. Mirrors scripts the OSS-Fuzz base image expects.
cd "$SRC/rxls"

cargo fuzz build -O --fuzz-dir fuzz

TARGET_DIR="fuzz/target/x86_64-unknown-linux-gnu/release"
for target in parse author edit; do
  cp "$TARGET_DIR/$target" "$OUT/"
done

# Seed corpus: the committed fixtures + any fetched reference files.
mkdir -p "$OUT/parse_seed_corpus"
find tests/fixtures -type f 2>/dev/null -exec cp {} "$OUT/parse_seed_corpus/" \; || true
zip -j "$OUT/parse_seed_corpus.zip" "$OUT/parse_seed_corpus/"* 2>/dev/null || true
