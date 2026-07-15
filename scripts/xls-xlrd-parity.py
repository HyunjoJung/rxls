#!/usr/bin/env python3
"""Reproducible parity harness: rxls vs the xlrd reference implementation.

xlrd (the canonical Python `.xls` reader) is used as an independent oracle —
NOT the production POI golden — so this measures rxls against a real reference
parser, including Excel-faithful date rendering (xlrd `xldate`).

For each `.xls` in the corpus it renders an xlrd "golden" text the same way rxls
does (cells in row/col order; dates as ISO; literal-aware percent ×100; numbers
whole->int) and compares it to the rxls `extract` example output with a
whitespace-insensitive ratio. It also tallies rxls extraction coverage and the
files xlrd itself cannot read (a robustness datapoint for rxls).

Usage:
    python scripts/xls-xlrd-parity.py \
        --corpus local/xls-poc/xls_host \
        --bin    target/debug/examples/extract.exe

    python scripts/xls-xlrd-parity.py \
        --manifest local/public-corpus/manifest.json \
        --bin      target/debug/examples/extract \
        --limit    50

Requires: pip install xlrd>=2.0 . Exits non-zero if mean parity < --min (0.95).
The string comparison is bounded by `--max-compare-chars` so public-corpus
workbooks with very large extracted text do not make normalization or diffing
hang. Oversized exact matches can still enter the comparable set through a
bounded SHA-256 path capped by `--max-hash-chars`.
"""
import argparse
import difflib
import hashlib
import os
import re
import subprocess
import sys

from public_corpus_manifest import (
    corpus_files,
    emit_parity_provenance,
    manifest_files,
    report_path,
    report_reason,
    report_source_root,
    resolve_binary,
)

_CORPUS_FAILURE_RE = re.compile(
    r"^failure:\s+(?P<ext>\S+)\s+(?P<label>.*?)\s+"
    r"kind=(?P<kind>\S+)\s+decision=(?P<decision>\S+)\s+"
    r"evidence=(?P<evidence>\S+)\s+container=(?P<container>\S+)\s+"
    r"extension_mismatch=(?P<extension_mismatch>\S+)\s+(?P<error>.*)$"
)
_CORPUS_COMPARISON_EXCLUDE_DECISIONS = {
    "excluded_malformed_container",
    "excluded_malformed_workbook",
    "unsupported_encrypted",
    "unsupported_legacy_biff",
}


def _fnum(f):
    return str(int(f)) if (f == int(f) and abs(f) < 1e15) else repr(f)


def _strip_literals(fmt):
    """Drop quoted text, [color]/[locale] brackets, and escaped chars from a
    number-format code (matches rxls classify_string), lowercased."""
    out, it = [], iter(fmt)
    for c in it:
        if c == '"':
            for c2 in it:
                if c2 == '"':
                    break
        elif c == "[":
            for c2 in it:
                if c2 == "]":
                    break
        elif c in "\\_*":
            next(it, None)
        else:
            out.append(c.lower())
    return "".join(out)


def _is_percent(fmt):
    """True only if '%' survives literal-stripping (matches rxls classify_string)."""
    return "%" in _strip_literals(fmt)


def _is_elapsed_time(fmt):
    it = iter(fmt)
    for c in it:
        if c == '"':
            for c2 in it:
                if c2 == '"':
                    break
        elif c == "[":
            inner = []
            for c2 in it:
                if c2 == "]":
                    break
                inner.append(c2.lower())
            if inner and all(c in {"h", "m", "s"} for c in inner):
                return True
        elif c in "\\_*":
            next(it, None)
    return False


def _elapsed_time(value):
    total_seconds = round(value * 86_400)
    hours = total_seconds // 3_600
    minutes = (total_seconds % 3_600) // 60
    seconds = total_seconds % 60
    return f"{hours}:{minutes:02}:{seconds:02}"


def render_xlrd_cell_value(cell_type, value, fmt, datemode):
    try:
        import xlrd
        from xlrd.xldate import xldate_as_datetime
    except ImportError:
        sys.exit("xlrd not installed: pip install xlrd>=2.0")

    if cell_type in (xlrd.XL_CELL_EMPTY, xlrd.XL_CELL_BLANK):
        return None
    if cell_type == xlrd.XL_CELL_TEXT:
        return value
    if cell_type == xlrd.XL_CELL_DATE:
        if _is_elapsed_time(fmt):
            return _elapsed_time(value)
        # Date-vs-datetime is decided by the FORMAT, not the value's fraction
        # (a date-only format never shows time, even if the serial has one) —
        # matching Excel and rxls.
        low = _strip_literals(fmt)
        has_time = "h" in low or "s" in low
        has_date = "y" in low or "d" in low
        if datemode == 0 and value == 0.0 and has_date:
            return "00:00:00"
        dt = xldate_as_datetime(value, datemode)
        if has_date and has_time:
            return dt.strftime("%Y-%m-%d %H:%M:%S")
        if has_time:
            return dt.strftime("%H:%M:%S")
        return dt.strftime("%Y-%m-%d")
    if cell_type == xlrd.XL_CELL_NUMBER:
        return _fnum(value * 100) + "%" if _is_percent(fmt) else _fnum(value)
    if cell_type == xlrd.XL_CELL_BOOLEAN:
        return "TRUE" if value else "FALSE"
    if cell_type == xlrd.XL_CELL_ERROR:
        return xlrd.error_text_from_code.get(value, "#ERR!")
    return None


def xlrd_text(path):
    try:
        import xlrd
    except ImportError:
        sys.exit("xlrd not installed: pip install xlrd>=2.0")

    with open(os.devnull, "w") as devnull:
        book = xlrd.open_workbook(path, formatting_info=True, logfile=devnull)
    parts = []
    for sh in book.sheets():
        parts.append("# " + sh.name)
        for r in range(sh.nrows):
            row = []
            for c in range(sh.ncols):
                cell = sh.cell(r, c)
                t, v = cell.ctype, cell.value
                if t in (xlrd.XL_CELL_EMPTY, xlrd.XL_CELL_BLANK):
                    continue
                fmt = ""
                try:
                    fmt = book.format_map[book.xf_list[cell.xf_index].format_key].format_str
                except Exception:
                    pass
                s = render_xlrd_cell_value(t, v, fmt, book.datemode)
                if s is None:
                    continue
                if s != "":
                    row.append(s)
            if row:
                parts.append("\t".join(row))
    return "\n".join(parts)


def norm(s):
    return re.sub(r"\s+", "", s)


def hash_exact_match(left, right):
    if len(left) != len(right):
        return False
    left_hash = hashlib.sha256(left.encode("utf-8")).digest()
    right_hash = hashlib.sha256(right.encode("utf-8")).digest()
    return left_hash == right_hash


def parse_corpus_report(path):
    failures = []
    with open(path, encoding="utf-8") as report:
        for line in report:
            match = _CORPUS_FAILURE_RE.match(line.rstrip("\n"))
            if match:
                failures.append(match.groupdict())
    return failures


def _path_suffix(path):
    return str(path).replace("\\", "/")


def corpus_failure_for_path(path, corpus_failures):
    if not path or not corpus_failures:
        return None
    normalized = _path_suffix(path)
    best = None
    for failure in corpus_failures:
        label = _path_suffix(failure["label"])
        if normalized.endswith(label) and (best is None or len(label) > len(best["label"])):
            best = failure
    return best


def skip_classification(kind, reason, path=None, corpus_failures=None):
    if kind == "corpus-report-excluded":
        failure = corpus_failure_for_path(path, corpus_failures)
        if failure:
            return (failure["decision"], failure["evidence"], failure["kind"])
        return ("needs_oracle_triage", "missing_corpus_failure", None)
    if kind == "oversized-comparison":
        return ("needs_bounded_oracle", "comparison_budget_exceeded", None)
    if kind != "xlrd-unreadable":
        return ("needs_oracle_triage", "unknown_skip_kind", None)

    failure = corpus_failure_for_path(path, corpus_failures)
    if failure:
        return (failure["decision"], failure["evidence"], failure["kind"])
    if reason.startswith("XLRDError: Workbook is encrypted"):
        return ("unsupported_encrypted", "xlrd_encrypted_workbook", None)
    if reason.startswith("FormulaError:"):
        return (
            "documented_oracle_limitation",
            "xlrd_formula_parser_limitation",
            None,
        )
    if reason.startswith("TypeError: 'NoneType' object is not iterable"):
        return (
            "documented_oracle_limitation",
            "xlrd_parser_type_limitation",
            None,
        )
    if reason.startswith("AssertionError:"):
        return (
            "documented_oracle_limitation",
            "xlrd_assertion_limitation",
            None,
        )
    if reason.startswith("ValueError: cannot convert float NaN to integer"):
        return (
            "documented_oracle_limitation",
            "xlrd_nan_number_limitation",
            None,
        )
    if reason.startswith("XLRDError: Can't determine file's BIFF version") or (
        reason.startswith("XLRDError: Unsupported format, or corrupt file:")
        and "Expected BOF record" in reason
    ):
        return (
            "documented_oracle_limitation",
            "xlrd_biff_header_limitation",
            None,
        )
    if reason.startswith("CompDocError:"):
        return ("excluded_malformed_container", "xlrd_compdoc_error", None)
    if reason.startswith("error: unpack requires a buffer of"):
        return ("excluded_malformed_workbook", "xlrd_truncated_record", None)
    if reason.startswith("UnicodeDecodeError:") and "truncated data" in reason:
        return ("excluded_malformed_workbook", "xlrd_truncated_unicode", None)
    return ("needs_oracle_triage", "xlrd_exception", None)


def worst_records(records, limit):
    return sorted(records, key=lambda record: (record[0], record[1]))[: max(0, limit)]


def main():
    ap = argparse.ArgumentParser()
    source = ap.add_mutually_exclusive_group(required=True)
    source.add_argument("--corpus", help="dir of .xls files")
    source.add_argument("--manifest", help="public-corpus manifest.json")
    ap.add_argument("--bin", required=True, help="rxls `extract` example binary")
    ap.add_argument("--limit", type=int, default=None, help="maximum selected files")
    ap.add_argument(
        "--max-compare-chars",
        type=int,
        default=200_000,
        help="skip exact diff comparisons above this combined normalized length",
    )
    ap.add_argument(
        "--max-hash-chars",
        type=int,
        default=5_000_000,
        help="maximum combined text length eligible for oversized exact-hash admission",
    )
    ap.add_argument(
        "--corpus-report",
        default=None,
        help="optional `rxls corpus-report` output used to refine xlrd skip decisions",
    )
    ap.add_argument(
        "--show-skips",
        type=int,
        default=0,
        help="print this many skipped files with skip kind, decision, evidence, and reason",
    )
    ap.add_argument(
        "--show-worst",
        type=int,
        default=0,
        help="print this many lowest-parity comparable files with text lengths",
    )
    ap.add_argument("--min", type=float, default=0.95)
    args = ap.parse_args()

    emit_parity_provenance(
        args.manifest, oracle_reader="xlrd", package_distribution="xlrd"
    )

    binary = resolve_binary(args.bin)
    source_root = report_source_root(args.manifest, args.corpus)
    corpus_failures = parse_corpus_report(args.corpus_report) if args.corpus_report else []
    if args.manifest:
        files = manifest_files(args.manifest, {".xls"}, args.limit)
        print(f"manifest: {report_path(args.manifest)}")
    else:
        files = corpus_files(args.corpus, {".xls"}, args.limit)
    if not files:
        sys.exit("no .xls files selected")

    sims, rxls_ok, xlrd_failed, xlrd_failed_rxls_ok, oversized, hash_exact = [], 0, 0, 0, 0, 0
    records, skips = [], []
    by_skip_decision, by_skip_evidence, by_skip_corpus_kind = {}, {}, {}
    for f in files:
        rt = subprocess.run([binary, f], capture_output=True).stdout.decode("utf-8", "replace")
        if rt.strip():
            rxls_ok += 1
        failure = corpus_failure_for_path(f, corpus_failures)
        if failure and failure["decision"] in _CORPUS_COMPARISON_EXCLUDE_DECISIONS:
            skips.append(("corpus-report-excluded", f, failure["error"]))
            continue
        try:
            gold = xlrd_text(f)
        except Exception as e:  # noqa: BLE001
            xlrd_failed += 1
            if rt.strip():
                xlrd_failed_rxls_ok += 1
            skips.append(("xlrd-unreadable", f, f"{type(e).__name__}: {e}"))
            continue
        combined_text_len = len(gold) + len(rt)
        if combined_text_len > args.max_compare_chars:
            if combined_text_len > args.max_hash_chars:
                oversized += 1
                skips.append(
                    (
                        "oversized-comparison",
                        f,
                        f"combined text length {combined_text_len} exceeds {args.max_hash_chars} hash budget",
                    )
                )
                continue
            gold_norm = norm(gold)
            rxls_norm = norm(rt)
            if gold_norm and hash_exact_match(gold_norm, rxls_norm):
                hash_exact += 1
                sims.append(1.0)
                records.append((1.0, f, len(gold_norm), len(rxls_norm)))
                continue
            oversized += 1
            skips.append(
                (
                    "oversized-comparison",
                    f,
                    f"combined text length {combined_text_len} exceeds {args.max_compare_chars}",
                )
            )
            continue
        gold_norm = norm(gold)
        rxls_norm = norm(rt)
        combined_norm_len = len(gold_norm) + len(rxls_norm)
        if combined_norm_len > args.max_compare_chars:
            if combined_norm_len > args.max_hash_chars:
                oversized += 1
                skips.append(
                    (
                        "oversized-comparison",
                        f,
                        f"normalized text length {combined_norm_len} exceeds {args.max_hash_chars} hash budget",
                    )
                )
                continue
            if gold_norm and hash_exact_match(gold_norm, rxls_norm):
                hash_exact += 1
                sims.append(1.0)
                records.append((1.0, f, len(gold_norm), len(rxls_norm)))
                continue
            oversized += 1
            skips.append(
                (
                    "oversized-comparison",
                    f,
                    (
                        f"normalized text length {len(gold_norm) + len(rxls_norm)} "
                        f"exceeds {args.max_compare_chars}"
                    ),
                )
            )
            continue
        if not gold_norm:
            continue
        ratio = difflib.SequenceMatcher(None, gold_norm, rxls_norm).ratio()
        sims.append(ratio)
        records.append((ratio, f, len(gold_norm), len(rxls_norm)))

    if not sims:
        sys.exit("no comparable files after oracle/size filtering")
    mean = sum(sims) / len(sims)
    print(
        f"files: {len(files)}   rxls extracted: {rxls_ok}   "
        f"xlrd-unreadable: {xlrd_failed}   comparable: {len(sims)}   "
        f"oversized-comparisons: {oversized}   hash-exact-comparisons: {hash_exact}"
    )
    print(f"rxls vs xlrd: mean parity {mean*100:.3f}%   >=99%: {sum(v>=0.99 for v in sims)}/{len(sims)}")
    print(f"xlrd-unreadable with rxls output: {xlrd_failed_rxls_ok}/{xlrd_failed}")

    for kind, path, reason in skips:
        decision, evidence, corpus_kind = skip_classification(
            kind, reason, path=path, corpus_failures=corpus_failures
        )
        by_skip_decision[decision] = by_skip_decision.get(decision, 0) + 1
        by_skip_evidence[evidence] = by_skip_evidence.get(evidence, 0) + 1
        if corpus_kind:
            by_skip_corpus_kind[corpus_kind] = by_skip_corpus_kind.get(corpus_kind, 0) + 1
    for decision, count in sorted(by_skip_decision.items()):
        print(f"by_skip_decision: {decision} skipped={count}")
    for evidence, count in sorted(by_skip_evidence.items()):
        print(f"by_skip_evidence: {evidence} skipped={count}")
    for corpus_kind, count in sorted(by_skip_corpus_kind.items()):
        print(f"by_skip_corpus_kind: {corpus_kind} skipped={count}")
    for kind, path, reason in skips[: max(0, args.show_skips)]:
        decision, evidence, corpus_kind = skip_classification(
            kind, reason, path=path, corpus_failures=corpus_failures
        )
        corpus_part = f" corpus_kind={corpus_kind}" if corpus_kind else ""
        print(
            f"skip: kind={kind} decision={decision} evidence={evidence}{corpus_part} "
            f"path={report_path(path, source_root)} "
            f"reason={report_reason(reason, path, source_root)}"
        )
    for ratio, path, gold_chars, rxls_chars in worst_records(records, args.show_worst):
        failure = corpus_failure_for_path(path, corpus_failures)
        corpus_part = ""
        if failure:
            corpus_part = (
                f" decision={failure['decision']} evidence={failure['evidence']} "
                f"corpus_kind={failure['kind']}"
            )
        print(
            f"low-parity: ratio={ratio:.3f} gold_chars={gold_chars} "
            f"rxls_chars={rxls_chars}{corpus_part} "
            f"path={report_path(path, source_root)}"
        )
    sys.exit(0 if mean >= args.min else 1)


if __name__ == "__main__":
    main()
