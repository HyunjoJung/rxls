#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
requested_out=${1:-target/wasm-package}
out_parent=$(dirname "$requested_out")
out_name=$(basename "$requested_out")
case "$out_name" in
  "" | "/" | "." | "..")
    echo "refusing unsafe WASM package output: $requested_out" >&2
    exit 1
    ;;
esac
mkdir -p "$out_parent"
out_parent=$(cd "$out_parent" && pwd)
out="$out_parent/$out_name"
if [[ "$out" == "/" || "$out" == "$root" ]]; then
  echo "refusing unsafe WASM package output: $out" >&2
  exit 1
fi
staging=$(mktemp -d "$out_parent/.rxls-wasm-package.XXXXXX")
trap 'rm -rf "$staging"' EXIT

expected_bindgen=$(python3 -c 'import pathlib,tomllib; p=tomllib.loads(pathlib.Path("bindings/wasm/Cargo.lock").read_text()); print(next(x["version"] for x in p["package"] if x["name"] == "wasm-bindgen"))')
actual_bindgen=$(wasm-bindgen --version | awk '{print $2}')
if [[ "$actual_bindgen" != "$expected_bindgen" ]]; then
  echo "wasm-bindgen CLI $actual_bindgen does not match Cargo.lock $expected_bindgen" >&2
  exit 1
fi

cargo build --manifest-path bindings/wasm/Cargo.toml \
  --target wasm32-unknown-unknown --release --locked

mkdir -p "$staging/node" "$staging/web" "$staging/demo"
wasm-bindgen bindings/wasm/target/wasm32-unknown-unknown/release/rxls_wasm.wasm \
  --target nodejs --typescript --out-name rxls_wasm --out-dir "$staging/node"
wasm-bindgen bindings/wasm/target/wasm32-unknown-unknown/release/rxls_wasm.wasm \
  --target web --typescript --out-name rxls_wasm --out-dir "$staging/web"
cp bindings/wasm/npm/package.json "$staging/package.json"
cp bindings/wasm/npm/web-package.json "$staging/web/package.json"
cp bindings/wasm/npm/README.md "$staging/README.md"
cp LICENSE "$staging/LICENSE"
cp bindings/wasm/demo/index.html bindings/wasm/demo/app.js "$staging/demo/"
python3 scripts/check_wasm_package.py "$staging"
rm -rf "$out"
mv "$staging" "$out"
trap - EXIT
