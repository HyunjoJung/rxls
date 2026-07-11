---
name: Bug report
about: Wrong/garbled text, a wrong date/number, a parse failure, or a crash
title: ''
labels: bug
assignees: ''
---

## What happened

A clear, concise description of the problem.

## Reproduction

1. The `.xls` file that triggers it (attach it if you can — even a minimal one)
2. The code / CLI you ran (e.g. `rxls::extract_text(&bytes)`)
3. The error, panic message, or wrong output (cell, sheet, value)

## Expected vs actual

- **Expected:** what the text should be (e.g. what Excel / xlrd / POI produces)
- **Actual:** what `rxls` produced

## Environment

- `rxls` version:
- Rust version (`rustc --version`):
- OS:

## Additional context

BIFF generation (BIFF8 vs BIFF5/7), workbook codepage, date system (1900/1904),
encryption — anything that helps narrow it down.
