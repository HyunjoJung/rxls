#!/usr/bin/env python3
"""Verify public-corpus reports and README claims against one checked-in baseline."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BASELINE = ROOT / "tests" / "oracles" / "public-corpus-baseline.json"
SCHEMA = "rxls.public-corpus-baseline.v1"
README_START = "<!-- public-corpus-baseline:start -->"
README_END = "<!-- public-corpus-baseline:end -->"
ORACLE_READERS = {
    "xls": "xlrd",
    "ooxml": "openpyxl",
    "xlsb": "pyxlsb",
    "ods": "xml.etree.ElementTree",
}


def load_baseline(path: Path) -> dict:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema") != SCHEMA:
        raise ValueError(f"unexpected baseline schema: {payload.get('schema')!r}")
    return payload


def _integer_line(text: str, label: str) -> int | None:
    match = re.search(rf"^{re.escape(label)}:\s+(\d+)\s*$", text, re.MULTILINE)
    return int(match.group(1)) if match else None


def verify_corpus(text: str, expected: dict) -> list[str]:
    errors: list[str] = []
    for key in [
        "manifest_files",
        "eligible_files",
        "opened",
        "failed",
        "expected_rejections",
        "unexpected_failures",
        "unexpected_accepts",
        "skipped",
    ]:
        actual = _integer_line(text, key)
        if actual != expected[key]:
            errors.append(f"corpus {key}: expected {expected[key]}, found {actual}")

    rows = {
        match.group("ext"): {
            "files": int(match.group("files")),
            "opened": int(match.group("opened")),
            "failed": int(match.group("failed")),
        }
        for match in re.finditer(
            r"^by_ext:\s+(?P<ext>\.\w+)\s+files=(?P<files>\d+)\s+"
            r"opened=(?P<opened>\d+)\s+failed=(?P<failed>\d+)\s*$",
            text,
            re.MULTILINE,
        )
    }
    for ext, expected_row in expected["by_ext"].items():
        if rows.get(ext) != expected_row:
            errors.append(f"corpus by_ext {ext}: expected {expected_row}, found {rows.get(ext)}")
    return errors


def _summary(text: str) -> dict[str, int] | None:
    match = re.search(
        r"^files:\s+(?P<files>\d+)\s+rxls extracted:\s+(?P<rxls>\d+)\s+.*?"
        r"comparable:\s+(?P<comparable>\d+)(?:\s|$)",
        text,
        re.MULTILINE,
    )
    if not match:
        return None
    return {
        "files": int(match.group("files")),
        "rxls_extracted": int(match.group("rxls")),
        "comparable": int(match.group("comparable")),
    }


def _metric(text: str, kind: str) -> tuple[float, int | None, int | None] | None:
    labels = {
        "xls": r"rxls vs xlrd: mean parity",
        "ooxml": r"rxls vs openpyxl: mean parity",
        "xlsb": r"rxls vs pyxlsb: mean parity",
        "ods": r"rxls vs ODS visible oracle: mean recall",
    }
    if kind in {"xls", "ooxml"}:
        match = re.search(
            rf"^{labels[kind]}\s+(?P<mean>\d+(?:\.\d+)?)%\s+"
            r">=99%:\s+(?P<count>\d+)/(?P<total>\d+)\s*$",
            text,
            re.MULTILINE,
        )
        if not match:
            return None
        return (
            float(match.group("mean")),
            int(match.group("count")),
            int(match.group("total")),
        )
    match = re.search(
        rf"^{labels[kind]}\s+(?P<mean>\d+(?:\.\d+)?)%\s+over\s+"
        r"(?P<total>\d+) files\s*$",
        text,
        re.MULTILINE,
    )
    if not match:
        return None
    return float(match.group("mean")), None, int(match.group("total"))


def verify_parity(text: str, kind: str, expected: dict) -> list[str]:
    errors: list[str] = []
    provenance = re.search(
        r"^provenance: oracle_reader=(?P<reader>\S+) "
        r"oracle_version=(?P<version>\S+)\s*$",
        text,
        re.MULTILINE,
    )
    if provenance is None:
        errors.append(f"{kind} oracle provenance is missing")
    else:
        reader = provenance.group("reader")
        version = provenance.group("version")
        if reader != ORACLE_READERS[kind]:
            errors.append(
                f"{kind} oracle reader: expected {ORACLE_READERS[kind]}, found {reader}"
            )
        if version == "unavailable":
            errors.append(f"{kind} oracle version is unavailable")

    manifest_digest = re.search(
        r"^provenance: input_manifest_sha256=(?P<digest>\S+)\s*$",
        text,
        re.MULTILINE,
    )
    if manifest_digest is None or re.fullmatch(
        r"[0-9a-f]{64}", manifest_digest.group("digest")
    ) is None:
        errors.append(f"{kind} input manifest SHA-256 is missing or invalid")

    summary = _summary(text)
    expected_summary = {
        key: expected[key] for key in ["files", "rxls_extracted", "comparable"]
    }
    if summary != expected_summary:
        errors.append(f"{kind} summary: expected {expected_summary}, found {summary}")

    metric = _metric(text, kind)
    if metric is None:
        errors.append(f"{kind} metric line is missing")
        return errors
    mean, at_least_99, total = metric
    if abs(mean - float(expected["mean_percent"])) > 0.0005:
        errors.append(
            f"{kind} mean_percent: expected {expected['mean_percent']:.3f}, found {mean:.3f}"
        )
    if total != expected["comparable"]:
        errors.append(f"{kind} metric total: expected {expected['comparable']}, found {total}")
    expected_at_least = expected.get("at_least_99")
    if expected_at_least is not None and at_least_99 != expected_at_least:
        errors.append(
            f"{kind} at_least_99: expected {expected_at_least}, found {at_least_99}"
        )
    return errors


def readme_block(baseline: dict) -> str:
    corpus = baseline["corpus"]
    parity = baseline["parity"]
    by_ext = corpus["by_ext"]
    return "\n".join(
        [
            README_START,
            f"**Current public-corpus gate ({baseline['as_of']}).** The pinned fetch recipe selects {corpus['manifest_files']}",
            f"files from Apache POI and calamine at immutable upstream commits: {by_ext['.xls']['files']} `.xls`,",
            f"{by_ext['.xlsx']['files']} `.xlsx`, {by_ext['.xlsm']['files']} `.xlsm`, {by_ext['.xlsb']['files']} `.xlsb`, and {by_ext['.ods']['files']} `.ods`. `rxls corpus-report` opens",
            f"{corpus['opened']}; the remaining {corpus['expected_rejections']} are explicit expected rejections for encrypted input,",
            "unsupported legacy BIFF, malformed containers, or structurally invalid BIFF streams.",
            f"The report records {corpus['unexpected_failures']} unexpected failures and {corpus['unexpected_accepts']} unexpected accepts. Public visible-value checks report:",
            "",
            "| Format | Comparable files | Result |",
            "|---|---:|---:|",
            f"| `.xls` vs `xlrd` | {parity['xls']['comparable']} | {parity['xls']['mean_percent']:.3f}% mean parity; {parity['xls']['at_least_99']}/{parity['xls']['comparable']} at least 99% |",
            f"| `.xlsx`/`.xlsm` vs `openpyxl` | {parity['ooxml']['comparable']} | {parity['ooxml']['mean_percent']:.3f}% mean parity; {parity['ooxml']['at_least_99']}/{parity['ooxml']['comparable']} at least 99% |",
            f"| `.xlsb` vs `pyxlsb` plus committed residual oracles | {parity['xlsb']['comparable']} | {parity['xlsb']['mean_percent']:.3f}% mean parity |",
            f"| `.ods` vs bounded ODF XML visible-text oracle | {parity['ods']['comparable']} | {parity['ods']['mean_percent']:.3f}% mean recall |",
            README_END,
        ]
    )


def verify_readme(text: str, baseline: dict) -> list[str]:
    expected = readme_block(baseline)
    start = text.find(README_START)
    end = text.find(README_END, start + len(README_START)) if start >= 0 else -1
    if start < 0 or end < 0:
        return ["README public-corpus baseline markers are missing"]
    actual = text[start : end + len(README_END)]
    return [] if actual == expected else ["README public-corpus block differs from baseline"]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    parser.add_argument("--corpus-report", type=Path)
    parser.add_argument("--xls", type=Path)
    parser.add_argument("--ooxml", type=Path)
    parser.add_argument("--xlsb", type=Path)
    parser.add_argument("--ods", type=Path)
    parser.add_argument("--readme", type=Path)
    parser.add_argument("--render-readme", action="store_true")
    args = parser.parse_args(argv)
    try:
        baseline = load_baseline(args.baseline)
        if args.render_readme:
            print(readme_block(baseline))
            return 0

        errors: list[str] = []
        if args.corpus_report:
            errors.extend(
                verify_corpus(
                    args.corpus_report.read_text(encoding="utf-8"), baseline["corpus"]
                )
            )
        for kind in ["xls", "ooxml", "xlsb", "ods"]:
            path = getattr(args, kind)
            if path:
                errors.extend(
                    verify_parity(
                        path.read_text(encoding="utf-8"), kind, baseline["parity"][kind]
                    )
                )
        if args.readme:
            errors.extend(verify_readme(args.readme.read_text(encoding="utf-8"), baseline))
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"public baseline: {error}", file=sys.stderr)
        return 2

    if errors:
        for error in errors:
            print(f"public baseline: {error}", file=sys.stderr)
        return 1
    print("public baseline: verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
