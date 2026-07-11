#!/usr/bin/env python3
"""Parity of rxls `.ods` extraction vs visible ODF cell text.

    cargo build --features ods --example extract
    python3 scripts/ods-odfpy-parity.py --corpus local/ods \
        --bin target/debug/examples/extract

    python3 scripts/ods-odfpy-parity.py \
        --manifest local/public-corpus/manifest.json \
        --bin target/debug/examples/extract \
        --limit 50

Exits non-zero if mean recall < --min (0.90). The oracle reads `content.xml`
directly so formatted date/time and styled numeric cells are compared using the
same visible text contract as rxls extraction. Multiset recall is case- and
whitespace-insensitive.
"""
import argparse
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from zipfile import ZipFile
from collections import Counter

from oracle_timeout import run_with_timeout
from public_corpus_manifest import corpus_files, manifest_files, resolve_binary

MAX_ORACLE_VALUES = 100_000
ORACLE_LIMIT_ERROR = "ODS oracle value limit exceeded"


def local_name(name: str) -> str:
    return name.rsplit("}", 1)[-1]


def attr(elem: ET.Element, name: str) -> str | None:
    for key, value in elem.attrib.items():
        if local_name(key) == name:
            return value
    return None


def repeat_count(elem: ET.Element, name: str) -> int:
    try:
        return max(1, int(attr(elem, name) or "1"))
    except ValueError:
        return 1


def require_value_capacity(current: int, additional: int) -> None:
    if additional > MAX_ORACLE_VALUES - current:
        raise RuntimeError(ORACLE_LIMIT_ERROR)


def append_odf_text(elem: ET.Element, out: list[str]) -> None:
    if elem.text:
        out.append(elem.text)
    for child in elem:
        tag = local_name(child.tag)
        if tag == "s":
            out.append(" " * repeat_count(child, "c"))
        elif tag == "tab":
            out.append("\t")
        elif tag == "line-break":
            out.append("\n")
        else:
            append_odf_text(child, out)
        if child.tail:
            out.append(child.tail)


def cell_display_text(cell: ET.Element) -> str:
    paragraphs = []
    for child in cell:
        tag = local_name(child.tag)
        if tag == "annotation":
            continue
        if tag != "p":
            continue
        parts: list[str] = []
        append_odf_text(child, parts)
        paragraphs.append("".join(parts))
    return "".join(paragraphs).strip()


def cell_oracle_value(cell: ET.Element) -> str | None:
    value_type = attr(cell, "value-type") or ""
    if value_type == "boolean":
        value = attr(cell, "boolean-value")
        if value is not None:
            return value.lower()

    display = cell_display_text(cell)
    if display:
        return display

    for name in ["value", "date-value", "time-value", "string-value"]:
        value = attr(cell, name)
        if value:
            return value.strip()
    return None


def visible_ods_values(path: str) -> list[str]:
    """Return ODS cell values in the text form rxls extraction compares."""
    with ZipFile(path) as archive:
        content = archive.read("content.xml")
    root = ET.fromstring(content)
    out: list[str] = []
    for table in root.iter():
        if local_name(table.tag) != "table":
            continue
        for row in table:
            if local_name(row.tag) != "table-row":
                continue
            row_values: list[str] = []
            for cell in row:
                if local_name(cell.tag) not in {"table-cell", "covered-table-cell"}:
                    continue
                value = cell_oracle_value(cell)
                if value is None:
                    continue
                repeat = repeat_count(cell, "number-columns-repeated")
                require_value_capacity(len(row_values), repeat)
                row_values.extend([value] * repeat)
            if not row_values:
                continue
            row_repeat = repeat_count(row, "number-rows-repeated")
            require_value_capacity(len(out), len(row_values) * row_repeat)
            for _ in range(row_repeat):
                out.extend(row_values)
    return out


def oracle(path: str) -> list:
    return [value.lower() for value in visible_ods_values(path)]


def rxls_tokens(text: str) -> list:
    toks = []
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        for t in line.split("\t"):
            t = t.strip()
            if t:
                toks.append(t.lower())
    return toks


def canon(tok: str) -> str:
    """Canonicalize numerics so `0.0` == `0` (oracle vs rxls integer rendering)."""
    try:
        return repr(float(tok))
    except ValueError:
        return tok


def recall(orc: list, got: list) -> float:
    if not orc:
        return 1.0
    co = Counter(canon(t) for t in orc)
    cg = Counter(canon(t) for t in got)
    return sum(min(n, cg[k]) for k, n in co.items()) / len(orc)


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
    if kind == "oracle-timeout":
        return ("needs_bounded_oracle", "ods_oracle_timeout", None)
    if kind != "oracle-error":
        return ("needs_oracle_triage", "unknown_skip_kind", None)
    if reason.startswith("RuntimeError: ODS oracle value limit exceeded"):
        return ("documented_bounded_oracle", "ods_repeated_value_limit", None)
    if reason.startswith("ParseError:"):
        failure = corpus_failure_for_path(path, corpus_failures)
        if failure:
            return (failure["decision"], failure["evidence"], failure["kind"])
        return ("needs_corpus_crosscheck", "ods_parse_error", None)
    if reason.startswith("BadZipFile: File is not a zip file"):
        failure = corpus_failure_for_path(path, corpus_failures)
        if failure:
            return (failure["decision"], failure["evidence"], failure["kind"])
        return ("needs_corpus_crosscheck", "ods_non_zip_container", None)
    return ("needs_oracle_triage", "ods_oracle_exception", None)


def main() -> None:
    ap = argparse.ArgumentParser()
    source = ap.add_mutually_exclusive_group(required=True)
    source.add_argument("--corpus", help="dir of .ods files")
    source.add_argument("--manifest", help="public-corpus manifest.json")
    ap.add_argument("--bin", required=True)
    ap.add_argument("--limit", type=int, default=None, help="maximum selected files")
    ap.add_argument(
        "--oracle-timeout-seconds",
        type=float,
        default=10.0,
        help="skip one file when the bounded ODF XML oracle exceeds this timeout",
    )
    ap.add_argument(
        "--corpus-report",
        default=None,
        help="optional `rxls corpus-report` output used to refine ODS oracle skip decisions",
    )
    ap.add_argument(
        "--show-skips",
        type=int,
        default=0,
        help="print this many skipped files with skip kind, decision, evidence, and reason",
    )
    ap.add_argument("--min", type=float, default=0.90)
    args = ap.parse_args()

    binary = resolve_binary(args.bin)
    corpus_failures = parse_corpus_report(args.corpus_report) if args.corpus_report else []
    if args.manifest:
        files = manifest_files(args.manifest, {".ods"}, args.limit)
        print(f"manifest: {args.manifest}")
    else:
        files = corpus_files(args.corpus, {".ods"}, args.limit)
    if not files:
        sys.exit("no .ods files selected")
    ratios = []
    skips = []
    by_skip_decision, by_skip_evidence, by_skip_corpus_kind = {}, {}, {}
    rxls_ok = 0
    oracle_failed = 0
    for f in files:
        oracle_result = run_with_timeout(
            oracle,
            (f,),
            timeout_seconds=args.oracle_timeout_seconds,
        )
        if oracle_result.status == "timeout":
            oracle_failed += 1
            reason = f"oracle timeout after {args.oracle_timeout_seconds:g}s"
            skips.append(("oracle-timeout", f, reason))
            print(f"  oracle-skip {f}: {reason}")
            continue
        if oracle_result.status == "error":
            oracle_failed += 1
            skips.append(("oracle-error", f, oracle_result.error))
            print(f"  oracle-skip {f}: {oracle_result.error}")
            continue
        orc = oracle_result.value
        got = subprocess.run(
            [binary, f], capture_output=True, text=True, encoding="utf-8"
        ).stdout
        if got.strip():
            rxls_ok += 1
        r = recall(orc, rxls_tokens(got))
        ratios.append(r)
        print(f"  {f}: {r:.3f} ({len(orc)} values)")
    if not ratios:
        sys.exit("no comparable files")
    mean = sum(ratios) / len(ratios)
    print(
        f"files: {len(files)}   rxls extracted: {rxls_ok}   "
        f"oracle-skipped: {oracle_failed}   comparable: {len(ratios)}"
    )
    print(f"rxls vs ODS visible oracle: mean recall {mean:.3%} over {len(ratios)} files")

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
