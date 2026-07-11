#!/usr/bin/env bash
# Fetch real .xls reference test files (incl. genuine BIFF5/7 and date/format
# edge cases) from the calamine project's MIT-licensed test suite, so the rxls
# BIFF5 + date-rendering paths can be validated on REAL data rather than only
# synthetic unit tests.
#
#   bash scripts/fetch-xls-reference.sh
#   python scripts/xls-xlrd-parity.py --corpus local/xls-poc/cal \
#       --bin target/debug/examples/extract.exe
#
# For the broader public corpus across .xls/.xlsx/.xlsb/.ods, use:
#
#   python scripts/fetch-public-corpus.py
#
# Files are downloaded to local/xls-poc/cal/ (gitignored). Source:
# https://github.com/tafia/calamine/tree/master/tests
set -euo pipefail

DEST="local/xls-poc/cal"
BASE="https://raw.githubusercontent.com/tafia/calamine/master/tests"
mkdir -p "$DEST"

# BIFF5/7 (Book stream) + date/time/format + SST-CONTINUE edge cases.
FILES=(
  biff5-rich-text-string.xls
  biff5_write.xls
  misc_biff5_parsing.xls
  issue_643_biff5_formula.xls
  date.xls
  date_1904.xls
  formula-date-format.xls
  sst_continue.xls
  merge_cells.xls
  sheet_name_parsing.xls
  any_sheets.xls
  optional_records.xls
  xls_formula.xls
)

for f in "${FILES[@]}"; do
  curl -fsSL "$BASE/$f" -o "$DEST/$f" && echo "  fetched $f" || echo "  FAILED $f"
done
echo "done -> $DEST ($(ls "$DEST"/*.xls 2>/dev/null | wc -l) files)"

# A real .xlsb (BIFF12) reference for the `xlsb` reader parity vs pyxlsb.
XLSB_DEST="local/xlsb"
mkdir -p "$XLSB_DEST"
for f in issues.xlsb; do
  curl -fsSL "$BASE/$f" -o "$XLSB_DEST/$f" && echo "  fetched $f" || echo "  FAILED $f"
done

# A real .ods (OpenDocument) reference for the `ods` reader parity vs odfpy.
ODS_DEST="local/ods"
mkdir -p "$ODS_DEST"
for f in issues.ods; do
  curl -fsSL "$BASE/$f" -o "$ODS_DEST/$f" && echo "  fetched $f" || echo "  FAILED $f"
done
