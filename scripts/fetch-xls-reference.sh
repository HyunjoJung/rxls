#!/usr/bin/env bash
# Fetch a small, immutable calamine reference set for the CI parity smoke gate.
set -euo pipefail

REF="5d84fbf26de95324bd7d21b4aae77f649059bea1"
BASE="https://raw.githubusercontent.com/tafia/calamine/${REF}/tests"

echo "source_ref: calamine@${REF}"

mkdir -p local/xls-poc/cal local/xlsb local/ods

for file in \
  biff5-rich-text-string.xls \
  biff5_write.xls \
  misc_biff5_parsing.xls \
  issue_643_biff5_formula.xls \
  date.xls \
  date_1904.xls \
  formula-date-format.xls \
  sst_continue.xls \
  merge_cells.xls \
  sheet_name_parsing.xls \
  any_sheets.xls \
  optional_records.xls \
  xls_formula.xls
do
  curl --retry 3 --retry-all-errors -fsSL "$BASE/$file" -o "local/xls-poc/cal/$file"
done

curl --retry 3 --retry-all-errors -fsSL "$BASE/issues.xlsb" -o local/xlsb/issues.xlsb
curl --retry 3 --retry-all-errors -fsSL "$BASE/issues.ods" -o local/ods/issues.ods

python3 - <<'PY'
from hashlib import sha256
from pathlib import Path

expected = {
    "local/xls-poc/cal/any_sheets.xls": "efb61c9a60b826b18b251cd74bb80819fcf8eb566e74ef6130709dcd2f7e0590",
    "local/xls-poc/cal/biff5-rich-text-string.xls": "9cdb5240edb207c7f1a94eab394adecb75859ebe0536eb3ac9d6b186b0bf085e",
    "local/xls-poc/cal/biff5_write.xls": "1807130ee9d96c1a738772e53445f9c9efc193042a994549ec6d092e5386a5f9",
    "local/xls-poc/cal/date.xls": "e3eb3391a371cc1e0b85d94b9f1dd94bbcedf19733ed0ca449e2d269af758e9e",
    "local/xls-poc/cal/date_1904.xls": "331aa4058647a509cac802f988becde2c3963e7b6f34397585842e3e1d68fbfd",
    "local/xls-poc/cal/formula-date-format.xls": "a9a193980a55e727baaa04ab2aa22cc2ef31b6b47ffa88701a7c357161ff4c1c",
    "local/xls-poc/cal/issue_643_biff5_formula.xls": "a267d8ff22703ec53f5af56eb4d285e027acb6db3b66a0a7f4de9d7b55925d63",
    "local/xls-poc/cal/merge_cells.xls": "8900c1ed9184c3af774f56cc5d459377219603dd37cbfaa911fb6f759c780597",
    "local/xls-poc/cal/misc_biff5_parsing.xls": "db28e8dee09dd1ec4f9136b7cab9c06adb101ff0f037dcf6451c80c6bd049c96",
    "local/xls-poc/cal/optional_records.xls": "d28c87a27d3d3dbe243ee1c44a6672dd894bc2e2a378d908c96c93669d71236f",
    "local/xls-poc/cal/sheet_name_parsing.xls": "d9d18be74bfe03706943c97d0ee1d4b74299ecb0fb79c7b31c7b7e993c69d09f",
    "local/xls-poc/cal/sst_continue.xls": "b97ed9df56cf30d286502555fc9aa9430fa161c634c8fa238f320125b1ac7a24",
    "local/xls-poc/cal/xls_formula.xls": "553f0417bacbb34c7755cbe7334ecc63f850cb099d02af503143a9c679278a87",
    "local/xlsb/issues.xlsb": "06c648de5529e022ef399a2b6ebc227040e9fdf71a0780e19e614f556bdb53d7",
    "local/ods/issues.ods": "35b60d34e30e80b9931b5001ae9d8ff6ce0ac719983af8197a00ee59e01f07df",
}

failed = False
for name, wanted in expected.items():
    path = Path(name)
    actual = sha256(path.read_bytes()).hexdigest()
    print(f"sha256: {actual}  {name}")
    if actual != wanted:
        print(f"checksum mismatch: expected {wanted}", flush=True)
        failed = True
if failed:
    raise SystemExit(1)
PY
