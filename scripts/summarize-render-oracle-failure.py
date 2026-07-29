#!/usr/bin/env python3
"""Reduce failed Render Oracle reports to a bounded path-neutral summary."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any, Iterable


INPUT_SCHEMA = "rxls.libreoffice-render-parity.v1"
OUTPUT_SCHEMA = "rxls.render-oracle-failure-summary.v4"
OUTPUT_NAME = "render-oracle-failure-summary.json"
MAX_REPORT_BYTES = 256 * 1024 * 1024
MAX_TOTAL_BYTES = 768 * 1024 * 1024
MAX_OUTPUT_BYTES = 1024 * 1024
MAX_ROOT_ENTRIES = 128
MAX_JSON_DEPTH = 128
MAX_JSON_NODES = 2_000_000
MAX_JSON_INTEGER_DIGITS = 128
MAX_PAGE_COUNT = 64
SHARDS = 4

HEAD_RE = re.compile(r"[0-9a-f]{40}\Z")
HASH_RE = re.compile(r"[0-9a-f]{64}\Z")
CODE_RE = re.compile(r"[a-z][a-z0-9_]{0,95}\Z")
RAW_REPORT_RE = re.compile(
    r"(?:parity-report-|parity-[ab]-shard-|"
    r"authored-print-report|authored-print-shard-)"
)
STATUSES = frozenset({"compared", "different", "error", "skipped"})
FORMATS = frozenset({"ods", "xls", "xlsb", "xlsx"})
UNREVIEWED_CLASSIFICATION = "unreviewed_classification"
# This is deliberately a finite public vocabulary. It covers the stable terminal
# outcomes and command/runtime failures emitted by evaluate_case and the locked
# runtime smoke adapter. Any new or injected snake_case value is reduced to the
# fixed bucket until it receives an explicit privacy review here.
REVIEWED_CLASSIFICATIONS = frozenset(
    {
        "below_similarity_threshold",
        "input_limit",
        "libreoffice_adapter_profile_path_missing",
        "libreoffice_adapter_profile_setup_failed",
        "libreoffice_command_output_limit",
        "libreoffice_failed",
        "libreoffice_file_output_limit",
        "libreoffice_font_pack_mismatch",
        "libreoffice_not_found",
        "libreoffice_oracle_empty",
        "libreoffice_oracle_rejected",
        "libreoffice_timeout",
        "manifest_sha256_mismatch",
        "manifest_size_mismatch",
        "page_count_mismatch",
        "renderer_command_output_limit",
        "renderer_failed",
        "renderer_file_output_limit",
        "renderer_not_found",
        "renderer_pdf_type3_path_text_missing",
        "renderer_timeout",
        "semantic_content_one_sided",
        "unreadable_input",
        "visual_dependencies_missing",
        "within_threshold",
    }
)
# Unknown detailed classifications may still carry path- or content-derived
# fragments even when they satisfy CODE_RE. These ordered prefixes therefore
# select only a fixed, privacy-reviewed stage label; no part of the input value
# is copied into the output. Values outside these finite families remain in the
# single unreviewed bucket.
COARSE_CLASSIFICATION_PREFIXES = (
    (
        "authored_print_stage",
        ("authored_print_", "render_manifest_authored_"),
    ),
    (
        "input_stage",
        ("corpus_", "input_", "manifest_"),
    ),
    (
        "renderer_pdf_attestation_stage",
        ("renderer_pdf_font_", "renderer_pdf_type3_"),
    ),
    (
        "renderer_page_map_stage",
        (
            "renderer_pdf_page_count",
            "renderer_pdf_page_map",
            "renderer_print_pdf_",
        ),
    ),
    (
        "renderer_raster_stage",
        (
            "renderer_pdf_page_limit",
            "renderer_pdf_page_pixel_",
            "renderer_pdf_raster_",
            "renderer_pdf_total_pixel_",
        ),
    ),
    (
        "renderer_semantic_stage",
        ("renderer_semantic_", "semantic_svg_"),
    ),
    (
        "renderer_bundle_stage",
        ("live_output_", "render_manifest_"),
    ),
    (
        "renderer_stage",
        (
            "renderer_",
            "svg_",
        ),
    ),
    (
        "oracle_font_attestation_stage",
        ("libreoffice_font_",),
    ),
    (
        "oracle_adapter_stage",
        ("libreoffice_adapter_", "oracle_"),
    ),
    (
        "oracle_pdf_stage",
        ("libreoffice_pdf_",),
    ),
    (
        "oracle_raster_stage",
        (
            "libreoffice_page_",
            "libreoffice_raster_",
            "libreoffice_total_pixel_",
        ),
    ),
    (
        "oracle_stage",
        ("libreoffice_",),
    ),
    (
        "environment_stage",
        (
            "font_pack_",
            "numpy_",
            "pillow_",
            "poppler_",
            "visual_dependencies_",
        ),
    ),
    (
        "measurement_geometry_stage",
        ("pdfinfo_",),
    ),
    (
        "measurement_raster_stage",
        ("pdf_raster_", "pdftoppm_", "raster_"),
    ),
    (
        "measurement_semantic_stage",
        ("semantic_bbox_", "semantic_text_", "text_box_"),
    ),
    (
        "measurement_stage",
        (
            "artifact_",
            "comparison_",
            "metric_",
            "page_count_",
            "pdf_",
            "pdffonts_",
            "semantic_",
        ),
    ),
)
COARSE_CLASSIFICATIONS = frozenset(
    bucket for bucket, _ in COARSE_CLASSIFICATION_PREFIXES
)
OUTPUT_CLASSIFICATIONS = (
    REVIEWED_CLASSIFICATIONS
    | COARSE_CLASSIFICATIONS
    | {UNREVIEWED_CLASSIFICATION}
)
FEATURES = frozenset(
    {
        "border",
        "cell-fill",
        "chart",
        "chinese-text",
        "column-width",
        "conditional-format",
        "date-format",
        "explicit-row-height",
        "formula-cached",
        "hidden-column",
        "hidden-row",
        "image-drawing",
        "japanese-text",
        "korean-text",
        "latin-text",
        "merged-cells",
        "normal-font-carlito",
        "normal-font-noto",
        "normal-size-11",
        "normal-size-12",
        "noto-ofl-font",
        "number-cell",
        "ooxml-implicit-row",
        "percent-format",
        "print-settings",
        "right-to-left-layout",
        "row-height",
        "rtl-text",
        "sheet-format-missing",
        "sheet-format-present",
        "sparkline",
        "unicode-text",
        "wrapped-text",
    }
)
LABELS = ("authored-print", "parity-a", "parity-b")
MERGED = {
    "authored-print": "authored-print-report.json",
    "parity-a": "parity-report-a.json",
    "parity-b": "parity-report-b.json",
}
SHARDED = {
    "authored-print": "authored-print-shard-{index}.json",
    "parity-a": "parity-a-shard-{index}.json",
    "parity-b": "parity-b-shard-{index}.json",
}
CASES = {"full": 800, "ooxml-row-diagnostic": 12, "pilot": 40}
LANES = {
    "full": {"authored-print": 100, "parity-a": 800, "parity-b": 800},
    "ooxml-row-diagnostic": {
        "authored-print": 0,
        "parity-a": 12,
        "parity-b": 0,
    },
    "pilot": {"authored-print": 4, "parity-a": 40, "parity-b": 0},
}
REPORT_KEYS = {
    "configuration",
    "discovery",
    "files",
    "mode",
    "preflight",
    "schema",
    "summary",
}
DISCOVERY_KEYS = {
    "candidate_count",
    "pre_shard_selected_count",
    "selected_count",
    "shard_candidate_count",
    "shard_count",
    "shard_index",
    "truncated",
}


class SummaryError(RuntimeError):
    """A detailed report cannot be safely summarized."""


class _StrictJSONError(ValueError):
    """An input is not in the bounded JSON subset used by oracle evidence."""


def _json(value: object) -> bytes:
    return (
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=True) + "\n"
    ).encode()


def _strict_json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise _StrictJSONError("duplicate_key")
        result[key] = value
    return result


def _reject_json_constant(_: str) -> None:
    raise _StrictJSONError("non_finite_number")


def _reject_json_number(_: str) -> None:
    raise _StrictJSONError("non_integral_number")


def _parse_json_integer(token: str) -> int:
    digits = token[1:] if token.startswith("-") else token
    if len(digits) > MAX_JSON_INTEGER_DIGITS:
        raise _StrictJSONError("integer_limit")
    return int(token)


def _preflight_json_text(text: str) -> None:
    closers: list[str] = []
    structural_nodes = 0
    index = 0
    while index < len(text):
        character = text[index]
        if character == '"':
            index += 1
            while index < len(text):
                if text[index] == "\\":
                    index += 2
                    continue
                if text[index] == '"':
                    index += 1
                    break
                index += 1
            continue
        if character in "[{":
            structural_nodes += 1
            if structural_nodes > MAX_JSON_NODES:
                raise _StrictJSONError("json_complexity")
            closers.append("]" if character == "[" else "}")
            if len(closers) > MAX_JSON_DEPTH:
                raise _StrictJSONError("json_depth")
        elif character in "]}":
            if not closers or closers.pop() != character:
                raise _StrictJSONError("json_structure")
        elif character == ",":
            structural_nodes += 1
            if structural_nodes > MAX_JSON_NODES:
                raise _StrictJSONError("json_complexity")
        elif character == "-" or "0" <= character <= "9":
            start = index
            if character == "-":
                index += 1
            digit_start = index
            while index < len(text) and "0" <= text[index] <= "9":
                index += 1
            if index == digit_start:
                index = start + 1
                continue
            if index - digit_start > MAX_JSON_INTEGER_DIGITS:
                raise _StrictJSONError("integer_limit")
            if index < len(text) and text[index] in ".eE":
                raise _StrictJSONError("non_integral_number")
            continue
        index += 1
    if closers:
        raise _StrictJSONError("json_structure")


def _strict_json_loads(payload: bytes) -> object:
    try:
        text = payload.decode("utf-8")
        _preflight_json_text(text)
        return json.loads(
            text,
            object_pairs_hook=_strict_json_object,
            parse_constant=_reject_json_constant,
            parse_float=_reject_json_number,
            parse_int=_parse_json_integer,
        )
    except (UnicodeDecodeError, RecursionError, ValueError) as error:
        raise SummaryError("report_unreadable") from error


def _integer(value: object, code: str, maximum: int) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= maximum
    ):
        raise SummaryError(code)
    return value


def _page_count_pair(
    value: dict[str, Any], code: str
) -> tuple[int, int]:
    rxls_pages = _integer(value.get("rxls_pages"), code, MAX_PAGE_COUNT)
    libreoffice_pages = _integer(
        value.get("libreoffice_pages"), code, MAX_PAGE_COUNT
    )
    if rxls_pages == 0 or libreoffice_pages == 0 or rxls_pages == libreoffice_pages:
        raise SummaryError(code)
    return rxls_pages, libreoffice_pages


def _public_classification(value: str) -> str:
    if value in REVIEWED_CLASSIFICATIONS:
        return value
    for bucket, prefixes in COARSE_CLASSIFICATION_PREFIXES:
        if value.startswith(prefixes):
            return bucket
    return UNREVIEWED_CLASSIFICATION


def _count_map(
    value: object,
    total: int,
    code: str,
    allowed: frozenset[str] | None = None,
) -> dict[str, int]:
    if not isinstance(value, dict) or len(value) > 256:
        raise SummaryError(code)
    result: dict[str, int] = {}
    for key, raw_count in value.items():
        if (
            not isinstance(key, str)
            or CODE_RE.fullmatch(key) is None
            or (allowed is not None and key not in allowed)
        ):
            raise SummaryError(code)
        count = _integer(raw_count, code, total)
        if count == 0:
            raise SummaryError(code)
        result[key] = count
    if sum(result.values()) != total:
        raise SummaryError(code)
    return dict(sorted(result.items()))


def _read(path: Path, remaining: int) -> tuple[dict[str, Any], int]:
    try:
        metadata = path.lstat()
        if (
            not stat.S_ISREG(metadata.st_mode)
            or path.is_symlink()
            or not 0 < metadata.st_size <= min(MAX_REPORT_BYTES, remaining)
        ):
            raise SummaryError("report_type_or_size")
        payload = path.read_bytes()
        if len(payload) != metadata.st_size:
            raise SummaryError("report_type_or_size")
        value = _strict_json_loads(payload)
    except SummaryError:
        raise
    except OSError as error:
        raise SummaryError("report_unreadable") from error
    if not isinstance(value, dict):
        raise SummaryError("report_shape")
    return value, len(payload)


def _validate_report(
    value: dict[str, Any],
    *,
    profile: str,
    label: str,
    shard: int | None,
) -> tuple[list[dict[str, Any]], tuple[str, str]]:
    if (
        set(value) != REPORT_KEYS
        or value.get("schema") != INPUT_SCHEMA
        or value.get("mode") != "compare"
        or not isinstance(value.get("configuration"), dict)
        or not isinstance(value.get("preflight"), dict)
        or not isinstance(value.get("files"), list)
        or not isinstance(value.get("summary"), dict)
    ):
        raise SummaryError("report_schema")
    rows = value["files"]
    limit = LANES[profile][label]
    if len(rows) > limit:
        raise SummaryError("report_coverage")
    discovery = value.get("discovery")
    if not isinstance(discovery, dict) or set(discovery) != DISCOVERY_KEYS:
        raise SummaryError("discovery_shape")
    expected = {
        "candidate_count": CASES[profile],
        "pre_shard_selected_count": limit,
        "selected_count": len(rows),
        "shard_candidate_count": len(rows),
        "truncated": False,
    }
    if any(discovery.get(key) != expected_value for key, expected_value in expected.items()):
        raise SummaryError("discovery_coverage")
    if shard is None:
        if (
            discovery.get("shard_count") != 1
            or discovery.get("shard_index") != 0
            or len(rows) != limit
        ):
            raise SummaryError("discovery_merged")
    elif (
        discovery.get("shard_count") != SHARDS
        or discovery.get("shard_index") != shard
    ):
        raise SummaryError("discovery_shard")

    statuses: Counter[str] = Counter()
    classifications: Counter[str] = Counter()
    for row in rows:
        if not isinstance(row, dict):
            raise SummaryError("workbook_row")
        status = row.get("status")
        classification = row.get("classification")
        format_name = row.get("format")
        features = row.get("features")
        digest = row.get("sha256")
        if (
            not isinstance(status, str)
            or status not in STATUSES
            or not isinstance(classification, str)
            or CODE_RE.fullmatch(classification) is None
            or not isinstance(format_name, str)
            or format_name not in FORMATS
            or not isinstance(features, list)
            or len(features) > 256
            or any(
                not isinstance(feature, str) or feature not in FEATURES
                for feature in features
            )
            or features != sorted(set(features))
            or not isinstance(digest, str)
            or HASH_RE.fullmatch(digest) is None
        ):
            raise SummaryError("workbook_contract")
        if label == "authored-print" and (
            format_name != "xlsx" or "print-settings" not in features
        ):
            raise SummaryError("authored_print_contract")
        if profile == "ooxml-row-diagnostic" and (
            label != "parity-a"
            or format_name != "xlsx"
            or "ooxml-implicit-row" not in features
        ):
            raise SummaryError("ooxml_row_diagnostic_contract")
        if classification == "page_count_mismatch":
            if status != "error":
                raise SummaryError("page_count_diagnostic")
            _page_count_pair(row, "page_count_diagnostic")
        statuses[str(status)] += 1
        classifications[classification] += 1
    summary = value["summary"]
    if summary.get("files") != len(rows):
        raise SummaryError("summary_count")
    if _count_map(summary.get("by_status"), len(rows), "summary_status", STATUSES) != dict(
        sorted(statuses.items())
    ):
        raise SummaryError("summary_status")
    if _count_map(
        summary.get("by_classification"), len(rows), "summary_classification"
    ) != dict(sorted(classifications.items())):
        raise SummaryError("summary_classification")
    identities = tuple(
        hashlib.sha256(_json(value[key])).hexdigest()
        for key in ("configuration", "preflight")
    )
    return rows, identities


def _paths(root: Path, profile: str, label: str) -> list[tuple[Path, int | None]]:
    merged = root / MERGED[label]
    shards = [
        (root / SHARDED[label].format(index=index), index) for index in range(SHARDS)
    ]
    merged_present = merged.exists() or merged.is_symlink()
    present_shards = [
        item for item in shards if item[0].exists() or item[0].is_symlink()
    ]
    if merged_present and present_shards:
        raise SummaryError("report_fragment_ambiguity")
    if profile != "full" and present_shards:
        raise SummaryError("unsharded_profile")
    return [(merged, None)] if merged_present else present_shards


def _empty(label: str) -> dict[str, object]:
    return {
        "by_classification": {},
        "by_feature": {},
        "by_format": {},
        "by_status": {},
        "label": label,
        "page_count_mismatches": [],
        "workbooks": 0,
    }


def _summarize_label(
    root: Path, profile: str, label: str, remaining: int
) -> tuple[dict[str, object], int]:
    paths = _paths(root, profile, label)
    if LANES[profile][label] == 0:
        if paths:
            raise SummaryError("unexpected_report")
        return _empty(label), 0
    if not paths:
        return _empty(label), 0
    rows: list[dict[str, Any]] = []
    seen: set[str] = set()
    identity: tuple[str, str] | None = None
    consumed = 0
    for path, shard in paths:
        document, size = _read(path, remaining - consumed)
        fragment, fragment_identity = _validate_report(
            document, profile=profile, label=label, shard=shard
        )
        consumed += size
        if identity is None:
            identity = fragment_identity
        elif identity != fragment_identity:
            raise SummaryError("fragment_identity")
        for row in fragment:
            digest = str(row["sha256"])
            if digest in seen:
                raise SummaryError("duplicate_workbook")
            seen.add(digest)
            rows.append(row)
    if len(rows) > LANES[profile][label]:
        raise SummaryError("report_coverage")

    statuses: Counter[str] = Counter()
    classes: Counter[str] = Counter()
    formats: dict[str, Counter[str]] = {}
    features: dict[str, Counter[str]] = {}
    page_count_mismatches: Counter[tuple[int, int]] = Counter()
    for row in rows:
        status = str(row["status"])
        raw_code = str(row["classification"])
        code = _public_classification(raw_code)
        fmt = str(row["format"])
        statuses[status] += 1
        classes[code] += 1
        formats.setdefault(fmt, Counter())[code] += 1
        for feature in row["features"]:
            features.setdefault(str(feature), Counter())[code] += 1
        if raw_code == "page_count_mismatch":
            page_count_mismatches[
                _page_count_pair(row, "page_count_diagnostic")
            ] += 1

    def groups(values: dict[str, Counter[str]]) -> dict[str, object]:
        return {
            key: {
                "by_classification": dict(sorted(counts.items())),
                "workbooks": sum(counts.values()),
            }
            for key, counts in sorted(values.items())
        }

    return {
        "by_classification": dict(sorted(classes.items())),
        "by_feature": groups(features),
        "by_format": groups(formats),
        "by_status": dict(sorted(statuses.items())),
        "label": label,
        "page_count_mismatches": [
            {
                "libreoffice_pages": libreoffice_pages,
                "rxls_pages": rxls_pages,
                "workbooks": count,
            }
            for (rxls_pages, libreoffice_pages), count in sorted(
                page_count_mismatches.items()
            )
        ],
        "workbooks": len(rows),
    }, consumed


def _validate_namespace(root: Path) -> None:
    allowed = set(MERGED.values()) | {
        template.format(index=index)
        for template in SHARDED.values()
        for index in range(SHARDS)
    }
    try:
        entries = list(root.iterdir())
    except OSError as error:
        raise SummaryError("input_root") from error
    if len(entries) > MAX_ROOT_ENTRIES:
        raise SummaryError("input_root_entries")
    if any(RAW_REPORT_RE.match(item.name) and item.name not in allowed for item in entries):
        raise SummaryError("unexpected_report_name")


def _validate_output(value: object) -> None:
    """Ensure no unreviewed key or path-like string reached the final JSON."""

    top = {"baseline_mode", "head_sha", "profile", "reports", "schema"}
    report_keys = {
        "by_classification",
        "by_feature",
        "by_format",
        "by_status",
        "label",
        "page_count_mismatches",
        "workbooks",
    }
    if not isinstance(value, dict) or set(value) != top:
        raise SummaryError("output_contract")
    reports = value.get("reports")
    if (
        value.get("schema") != OUTPUT_SCHEMA
        or not isinstance(value.get("profile"), str)
        or value.get("profile") not in CASES
        or not isinstance(value.get("baseline_mode"), str)
        or value.get("baseline_mode") not in {"candidate", "verify"}
        or not isinstance(value.get("head_sha"), str)
        or HEAD_RE.fullmatch(str(value["head_sha"])) is None
        or not isinstance(reports, list)
        or len(reports) != len(LABELS)
        or tuple(row.get("label") for row in reports if isinstance(row, dict)) != LABELS
        or any(not isinstance(row, dict) or set(row) != report_keys for row in reports)
    ):
        raise SummaryError("output_contract")

    profile = str(value["profile"])
    for report in reports:
        total = _integer(
            report["workbooks"],
            "output_count",
            LANES[profile][str(report["label"])],
        )
        _count_map(report["by_status"], total, "output_status", STATUSES)
        report_classes = _count_map(
            report["by_classification"],
            total,
            "output_classification",
            OUTPUT_CLASSIFICATIONS,
        )
        page_count_mismatches = report["page_count_mismatches"]
        if (
            not isinstance(page_count_mismatches, list)
            or len(page_count_mismatches) > total
        ):
            raise SummaryError("output_page_count_diagnostic")
        previous_pair: tuple[int, int] | None = None
        page_count_workbooks = 0
        for mismatch in page_count_mismatches:
            if (
                not isinstance(mismatch, dict)
                or set(mismatch)
                != {"libreoffice_pages", "rxls_pages", "workbooks"}
            ):
                raise SummaryError("output_page_count_diagnostic")
            pair = _page_count_pair(
                mismatch, "output_page_count_diagnostic"
            )
            if previous_pair is not None and pair <= previous_pair:
                raise SummaryError("output_page_count_diagnostic")
            previous_pair = pair
            workbooks = _integer(
                mismatch["workbooks"],
                "output_page_count_diagnostic",
                total,
            )
            if workbooks == 0:
                raise SummaryError("output_page_count_diagnostic")
            page_count_workbooks += workbooks
        if page_count_workbooks != report_classes.get(
            "page_count_mismatch", 0
        ):
            raise SummaryError("output_page_count_diagnostic")
        for key, allowed in (("by_format", FORMATS), ("by_feature", FEATURES)):
            groups = report[key]
            if (
                not isinstance(groups, dict)
                or len(groups) > len(allowed)
                or any(
                    not isinstance(name, str) or name not in allowed
                    for name in groups
                )
            ):
                raise SummaryError("output_group")
            grouped_total = 0
            grouped_classes: Counter[str] = Counter()
            for group in groups.values():
                if (
                    not isinstance(group, dict)
                    or set(group) != {"by_classification", "workbooks"}
                ):
                    raise SummaryError("output_group")
                group_total = _integer(
                    group["workbooks"], "output_group", total
                )
                if group_total == 0:
                    raise SummaryError("output_group")
                group_classes = _count_map(
                    group["by_classification"],
                    group_total,
                    "output_group",
                    OUTPUT_CLASSIFICATIONS,
                )
                if key == "by_feature" and any(
                    count > report_classes.get(classification, 0)
                    for classification, count in group_classes.items()
                ):
                    raise SummaryError("output_group")
                grouped_classes.update(group_classes)
                grouped_total += group_total
            if key == "by_format" and (
                grouped_total != total
                or dict(sorted(grouped_classes.items())) != report_classes
            ):
                raise SummaryError("output_group")


def summarize(
    root: Path, *, profile: str, baseline_mode: str, head_sha: str
) -> dict[str, object]:
    if (
        not isinstance(profile, str)
        or profile not in CASES
        or not isinstance(baseline_mode, str)
        or baseline_mode not in {"candidate", "verify"}
        or (profile != "full" and baseline_mode != "verify")
        or not isinstance(head_sha, str)
        or HEAD_RE.fullmatch(head_sha) is None
    ):
        raise SummaryError("invocation")
    if root.exists() or root.is_symlink():
        metadata = root.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or root.is_symlink():
            raise SummaryError("input_root")
        _validate_namespace(root)
    consumed = 0
    reports = []
    for label in LABELS:
        report, size = _summarize_label(root, profile, label, MAX_TOTAL_BYTES - consumed)
        consumed += size
        reports.append(report)
    result = {
        "baseline_mode": baseline_mode,
        "head_sha": head_sha,
        "profile": profile,
        "reports": reports,
        "schema": OUTPUT_SCHEMA,
    }
    _validate_output(result)
    if len(_json(result)) > MAX_OUTPUT_BYTES:
        raise SummaryError("output_size")
    return result


def write_atomic(path: Path, value: object) -> None:
    if path.name != OUTPUT_NAME:
        raise SummaryError("output_name")
    if path.exists() or path.is_symlink():
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            raise SummaryError("output_type")
    _validate_output(value)
    payload = _json(value)
    if len(payload) > MAX_OUTPUT_BYTES:
        raise SummaryError("output_size")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            dir=path.parent, prefix=f".{path.name}.", delete=False
        ) as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
            temporary = output.name
        os.replace(temporary, path)
        temporary = None
    except OSError as error:
        raise SummaryError("output_write") from error
    finally:
        if temporary is not None:
            try:
                Path(temporary).unlink()
            except OSError:
                pass


def parse_args(argv: Iterable[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-root", type=Path, required=True)
    parser.add_argument("--profile", choices=sorted(CASES), required=True)
    parser.add_argument("--baseline-mode", choices=("candidate", "verify"), required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args(argv)


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        write_atomic(
            args.output,
            summarize(
                args.input_root,
                profile=args.profile,
                baseline_mode=args.baseline_mode,
                head_sha=args.head_sha,
            ),
        )
        return 0
    except (SummaryError, OSError) as error:
        code = str(error) if isinstance(error, SummaryError) else "filesystem"
        print(f"render-oracle-failure-summary: {code}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
