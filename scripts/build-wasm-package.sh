#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
root=$(cd -- "$script_dir/.." && pwd -P)

refuse_unsafe_output() {
  echo "refusing unsafe WASM package output: $1" >&2
  return 1
}

resolve_wasm_package_output() {
  local requested_out=${1:-target/wasm-package}
  local component candidate parent_relative parent_real target_real
  local last_index
  local -a components

  case "$requested_out" in
    "" | /* | */ | *//* | *\\*)
      refuse_unsafe_output "$requested_out"
      return 1
      ;;
    target/*) ;;
    *)
      refuse_unsafe_output "$requested_out"
      return 1
      ;;
  esac
  if [[ "$requested_out" =~ [[:cntrl:]] ]]; then
    refuse_unsafe_output "$requested_out"
    return 1
  fi

  IFS='/' read -r -a components <<< "$requested_out"
  if (( ${#components[@]} < 2 )) || [[ "${components[0]}" != "target" ]]; then
    refuse_unsafe_output "$requested_out"
    return 1
  fi
  for component in "${components[@]}"; do
    case "$component" in
      "" | "." | "..")
        refuse_unsafe_output "$requested_out"
        return 1
        ;;
    esac
  done

  # Reject every existing component before mkdir can follow a link outside target.
  candidate="$root"
  for component in "${components[@]}"; do
    candidate="$candidate/$component"
    if [[ -L "$candidate" ]] || [[ -e "$candidate" && ! -d "$candidate" ]]; then
      refuse_unsafe_output "$requested_out"
      return 1
    fi
  done

  parent_relative=${requested_out%/*}
  mkdir -p -- "$root/$parent_relative"

  # Recheck after creation, then prove the physical parent remains under target.
  candidate="$root"
  for component in "${components[@]}"; do
    candidate="$candidate/$component"
    if [[ -L "$candidate" ]] || [[ -e "$candidate" && ! -d "$candidate" ]]; then
      refuse_unsafe_output "$requested_out"
      return 1
    fi
  done
  target_real=$(cd -- "$root/target" && pwd -P)
  parent_real=$(cd -- "$root/$parent_relative" && pwd -P)
  if [[ "$target_real" != "$root/target" ]] || \
     [[ "$parent_real" != "$target_real" && "$parent_real" != "$target_real/"* ]]; then
    refuse_unsafe_output "$requested_out"
    return 1
  fi

  last_index=$((${#components[@]} - 1))
  printf '%s/%s\n' "$parent_real" "${components[$last_index]}"
}

main() {
  local actual_bindgen expected_bindgen
  local requested_out=${1:-target/wasm-package}
  local checked_out out out_parent staging staging_quoted

  cd -- "$root"
  out=$(resolve_wasm_package_output "$requested_out")
  out_parent=${out%/*}
  staging=$(mktemp -d "$out_parent/.rxls-wasm-package.XXXXXX")
  printf -v staging_quoted '%q' "$staging"
  trap "rm -rf -- $staging_quoted" EXIT

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
  cp THIRD_PARTY_LICENSES.md "$staging/THIRD_PARTY_LICENSES.md"
  cp bindings/wasm/THIRD_PARTY_NOTICES.txt "$staging/THIRD_PARTY_NOTICES.txt"
  cp bindings/wasm/demo/index.html bindings/wasm/demo/app.js \
    bindings/wasm/demo/style.css "$staging/demo/"
  python3 scripts/check_wasm_package.py "$staging"

  # The build can be long; reject a link or traversal introduced before replacement.
  checked_out=$(resolve_wasm_package_output "$requested_out")
  if [[ "$checked_out" != "$out" ]]; then
    refuse_unsafe_output "$requested_out"
    exit 1
  fi
  rm -rf -- "$out"
  mv -- "$staging" "$out"
  trap - EXIT
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
