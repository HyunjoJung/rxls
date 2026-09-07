---
name: Bug report
about: Incorrect data, parse/write/edit failure, package damage, or a crash
title: ''
labels: bug
assignees: ''
---

## What happened

A clear, concise description of the problem.

For vulnerabilities, use the private reporting channel in
[SECURITY.md](https://github.com/HyunjoJung/rxls/blob/main/.github/SECURITY.md).

## Reproduction

1. A minimal `.xls`, `.xlsx`, `.xlsm`, `.xlsb`, or `.ods` file, a script that
   generates it, or steps to reproduce it. Share only redistributable,
   non-confidential content; hidden sheets, properties, comments, and macros
   can also contain private data. A synthetic example is welcome.
2. The exact API or CLI command and enabled Cargo features, or viewer/extension
   steps for a UI problem
3. The error, panic, wrong value, or changed/dropped package part

## Expected vs actual

- **Expected:** what the result should be; include an independent reader
  comparison if available (not required)
- **Actual:** what `rxls` produced

## Environment

- `rxls` version:
- Rust version (`rustc --version`, if applicable):
- Enabled features:
- OS:
- Browser or VS Code version (if applicable):

## Additional context

Container/format version, workbook codepage, date system (1900/1904), encryption,
macro presence, and whether the operation was read, create, edit, evaluate, or export.
