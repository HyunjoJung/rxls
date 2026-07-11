#!/usr/bin/env python3
"""Parity of rxls `.xlsb` extraction vs `pyxlsb` (the `.xlsb` oracle).

    cargo build --features xlsb --example extract
    python3 scripts/xlsb-pyxlsb-parity.py --corpus local/xlsb \
        --bin target/debug/examples/extract

    python3 scripts/xlsb-pyxlsb-parity.py \
        --manifest local/public-corpus/manifest.json \
        --bin target/debug/examples/extract \
        --expected-values tests/oracles/xlsb-visible-values.json \
        --limit 50

Requires: pip install pyxlsb . Exits non-zero if mean parity < --min (0.90).
Whitespace- and case-insensitive (TRUE/FALSE vs True/False); the oracle is the
multiset of non-empty cell values, compared to the rxls text dump.
Known formatted-display rows can be supplied through --expected-values when
pyxlsb exposes only raw serial values.
"""
import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from decimal import Decimal, InvalidOperation
from pathlib import Path

from public_corpus_manifest import corpus_files, manifest_files, resolve_binary


def scientific_decimal_token(text: str) -> str | None:
    """Canonicalize scientific notation so equivalent numeric dumps compare equal."""
    if "e" not in text:
        return None
    try:
        number = Decimal(text)
    except InvalidOperation:
        return None
    if not number.is_finite():
        return None

    normalized = format(number.normalize(), "f")
    if "." in normalized:
        normalized = normalized.rstrip("0").rstrip(".")
    return "0" if normalized in {"", "-0"} else normalized


def token(value) -> str:
    """Normalize one oracle or rxls value for multiset comparison."""
    if isinstance(value, float) and value.is_integer():
        value = int(value)
    text = str(value).strip().lower()
    return scientific_decimal_token(text) or text


def load_expected_values(path: str | os.PathLike[str]) -> dict[str, list[str]]:
    """Load explicit expected display values keyed by public-corpus path suffix."""
    with open(path, encoding="utf-8") as fh:
        raw = json.load(fh)

    if not isinstance(raw, dict):
        raise ValueError("expected values JSON must be an object")

    expected: dict[str, list[str]] = {}
    for key, values in raw.items():
        if not isinstance(values, list):
            raise ValueError(f"expected values for {key!r} must be a list")
        normalized_key = str(key).replace("\\", "/").lstrip("./")
        expected[normalized_key] = [token(value) for value in values]
    return expected


def expected_values_for(path: str, expected: dict[str, list[str]]) -> list[str] | None:
    """Return explicit expected values when `path` ends with a configured key."""
    if not expected:
        return None

    candidates = {
        os.path.normpath(path).replace("\\", "/"),
        os.path.abspath(path).replace("\\", "/"),
        Path(path).as_posix(),
    }
    for key, values in expected.items():
        for candidate in candidates:
            if candidate == key or candidate.endswith(f"/{key}"):
                return values
    return None


def oracle_for(path: str, expected: dict[str, list[str]] | None = None) -> tuple[str, list]:
    """Return either explicit expected display values or the raw pyxlsb oracle."""
    values = expected_values_for(path, expected or {})
    if values is not None:
        return ("expected", values)
    return ("pyxlsb", oracle(path))


def oracle(path: str) -> list:
    """The multiset of non-empty cell values pyxlsb reads, as lowercased tokens."""
    try:
        import pyxlsb
    except ImportError:
        sys.exit("pyxlsb not installed: pip install pyxlsb")

    out = []
    with pyxlsb.open_workbook(path) as wb:
        for sn in wb.sheets:
            with wb.get_sheet(sn) as sh:
                for row in sh.rows():
                    for c in row:
                        if c.v is None:
                            continue
                        v = c.v
                        out.append(token(v))
    return out


def rxls_tokens(text: str) -> list:
    """rxls `extract` dump → value tokens (drop the `# sheet` header lines)."""
    toks = []
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        for t in line.split("\t"):
            t = t.strip()
            if t:
                toks.append(token(t))
    return toks


def recall(orc: list, got: list) -> float:
    """Fraction of oracle values present in the rxls output (multiset)."""
    if not orc:
        return 1.0
    co, cg = Counter(orc), Counter(got)
    matched = sum(min(n, cg[k]) for k, n in co.items())
    return matched / len(orc)


_CORPUS_FAILURE_RE = re.compile(
    r"^failure:\s+(?P<ext>\S+)\s+(?P<label>.*?)\s+"
    r"kind=(?P<kind>\S+)\s+decision=(?P<decision>\S+)\s+"
    r"evidence=(?P<evidence>\S+)\s+container=(?P<container>\S+)\s+"
    r"extension_mismatch=(?P<extension_mismatch>\S+)\s+(?P<error>.*)$"
)


def parse_corpus_report(path: str | os.PathLike[str]) -> list[dict[str, str]]:
    failures = []
    with open(path, encoding="utf-8") as report:
        for line in report:
            match = _CORPUS_FAILURE_RE.match(line.rstrip("\n"))
            if match:
                failures.append(match.groupdict())
    return failures


def _path_suffix(path: str | os.PathLike[str]) -> str:
    return str(path).replace("\\", "/")


def corpus_failure_for_path(path: str | os.PathLike[str] | None, corpus_failures):
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
    if kind != "pyxlsb-unreadable":
        return ("needs_oracle_triage", "unknown_skip_kind", None)
    if reason.startswith("KeyError: '\\x04'"):
        return (
            "documented_oracle_limitation",
            "pyxlsb_relationship_id_limitation",
            None,
        )
    if reason.startswith("BadZipFile: File is not a zip file"):
        failure = corpus_failure_for_path(path, corpus_failures)
        if failure:
            return (failure["decision"], failure["evidence"], failure["kind"])
        return ("needs_corpus_crosscheck", "pyxlsb_non_zip_container", None)
    return ("needs_oracle_triage", "pyxlsb_exception", None)


def main() -> None:
    ap = argparse.ArgumentParser()
    source = ap.add_mutually_exclusive_group(required=True)
    source.add_argument("--corpus", help="dir of .xlsb files")
    source.add_argument("--manifest", help="public-corpus manifest.json")
    ap.add_argument("--bin", required=True, help="rxls `extract` example binary")
    ap.add_argument("--limit", type=int, default=None, help="maximum selected files")
    ap.add_argument(
        "--expected-values",
        help="JSON map of public-corpus path suffixes to expected display values",
    )
    ap.add_argument(
        "--corpus-report",
        default=None,
        help="optional `rxls corpus-report` output used to refine non-ZIP pyxlsb skip decisions",
    )
    ap.add_argument(
        "--show-skips",
        type=int,
        default=0,
        help="print this many skipped files with skip kind, decision, evidence, and reason",
    )
    ap.add_argument("--min", type=float, default=0.95)
    args = ap.parse_args()

    binary = resolve_binary(args.bin)
    expected = load_expected_values(args.expected_values) if args.expected_values else {}
    corpus_failures = parse_corpus_report(args.corpus_report) if args.corpus_report else []
    if args.manifest:
        files = manifest_files(args.manifest, {".xlsb"}, args.limit)
        print(f"manifest: {args.manifest}")
    else:
        files = corpus_files(args.corpus, {".xlsb"}, args.limit)
    if not files:
        sys.exit("no .xlsb files selected")

    ratios = []
    skips = []
    by_skip_decision, by_skip_evidence, by_skip_corpus_kind = {}, {}, {}
    rxls_ok = 0
    oracle_failed = 0
    expected_used = 0
    for f in files:
        try:
            oracle_source, orc = oracle_for(f, expected)
        except Exception as e:  # noqa: BLE001
            oracle_failed += 1
            reason = f"{type(e).__name__}: {e}"
            skips.append(("pyxlsb-unreadable", f, reason))
            print(f"  oracle-skip {f}: {reason}")
            continue
        if oracle_source == "expected":
            expected_used += 1
        got = subprocess.run(
            [binary, f], capture_output=True, text=True, encoding="utf-8"
        ).stdout
        if got.strip():
            rxls_ok += 1
        r = recall(orc, rxls_tokens(got))
        ratios.append(r)
        print(f"  {f}: {r:.3f} ({len(orc)} values, oracle={oracle_source})")

    if not ratios:
        sys.exit("no comparable files")
    mean = sum(ratios) / len(ratios)
    print(
        f"files: {len(files)}   rxls extracted: {rxls_ok}   "
        f"pyxlsb-unreadable: {oracle_failed}   comparable: {len(ratios)}"
    )
    print(f"expected-overrides: {expected_used}")
    print(f"rxls vs pyxlsb: mean parity {mean:.3%} over {len(ratios)} files")

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
            f"path={path} reason={reason}"
        )
    sys.exit(0 if mean >= args.min else 1)


if __name__ == "__main__":
    main()
