---
name: Bug report
about: Incorrect data, parse/write/edit failure, package damage, or a crash
title: ''
labels: bug
assignees: ''
---

## What happened

A clear, concise description of the problem.

## Reproduction

1. A minimal `.xls`, `.xlsx`, `.xlsm`, `.xlsb`, or `.ods` file that triggers it
   (remove confidential data before attaching)
2. The exact API or CLI command and enabled Cargo features
3. The error, panic, wrong value, or changed/dropped package part

## Expected vs actual

- **Expected:** what the result should be and the independent reader used to verify it
- **Actual:** what `rxls` produced

## Environment

- `rxls` version:
- Rust version (`rustc --version`):
- Enabled features:
- OS:

## Additional context

Container/format version, workbook codepage, date system (1900/1904), encryption,
macro presence, and whether the operation was read, create, edit, evaluate, or export.
