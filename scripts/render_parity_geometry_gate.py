#!/usr/bin/env python3
"""Shared strict validator for path-neutral unique-text geometry evidence."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
from typing import Any, Sequence


CONTRACT_PATH = Path(__file__).with_name("compare-render-parity-runs.py")


def _load_contract() -> Any:
    name = "rxls_render_parity_geometry_contract"
    existing = sys.modules.get(name)
    if existing is not None:
        return existing
    spec = importlib.util.spec_from_file_location(name, CONTRACT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError("geometry_contract_unavailable")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


CONTRACT = _load_contract()


class GeometryContractError(RuntimeError):
    """Raw report geometry is missing, malformed, or over budget."""


def _nonnegative_integer(value: object) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > CONTRACT.MAX_UNIQUE_TEXT_GEOMETRY_ITEMS
    ):
        raise GeometryContractError("unique_text_geometry_count")
    return value


def validate_report_identity(
    configuration: dict[str, Any],
    preflight: dict[str, Any],
) -> None:
    """Validate the renderer identity shared by every repeatability report."""
    try:
        CONTRACT._validate_renderer_identity(
            configuration,
            preflight,
        )
    except CONTRACT.MalformedReport as error:
        raise GeometryContractError(str(error)) from error


def _report_limits(
    max_pages: int | None,
    max_histogram_buckets: int | None,
) -> tuple[int, int]:
    if max_pages is None:
        max_pages = CONTRACT.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_PAGES
    if max_histogram_buckets is None:
        max_histogram_buckets = (
            CONTRACT.MAX_UNIQUE_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS
        )
    if (
        isinstance(max_pages, bool)
        or not isinstance(max_pages, int)
        or max_pages < 0
        or isinstance(max_histogram_buckets, bool)
        or not isinstance(max_histogram_buckets, int)
        or max_histogram_buckets < 0
    ):
        raise GeometryContractError("unique_text_geometry_report_limit")
    return max_pages, max_histogram_buckets


def validate_report_geometry(
    files: Sequence[dict[str, Any]],
    *,
    max_pages: int | None = None,
    max_histogram_buckets: int | None = None,
) -> tuple[int, int]:
    """Validate every retained geometry object and both report-wide caps."""
    max_pages, max_histogram_buckets = _report_limits(
        max_pages,
        max_histogram_buckets,
    )
    geometry_pages = 0
    histogram_buckets = 0
    keys = CONTRACT.UNIQUE_TEXT_GEOMETRY_METRICS
    for row in files:
        if not isinstance(row, dict):
            raise GeometryContractError("unique_text_geometry_row")
        metric_bearing = row.get("status") in {"compared", "different"}
        raw_pages = row.get("pages")
        if metric_bearing and (
            not isinstance(raw_pages, list) or not raw_pages
        ):
            raise GeometryContractError("unique_text_geometry_pages")
        if not isinstance(raw_pages, list):
            continue
        for page in raw_pages:
            if not isinstance(page, dict):
                raise GeometryContractError("unique_text_geometry_page")
            presence = tuple(key in page for key in keys)
            if presence == (False, False):
                if metric_bearing:
                    raise GeometryContractError(
                        "unique_text_geometry_pair"
                    )
                continue
            if presence != (True, True):
                raise GeometryContractError("unique_text_geometry_pair")
            geometry_pages += 1
            for key, prefix in (
                ("text_box_unique_geometry", "text_box"),
                ("text_line_box_unique_geometry", "text_line_box"),
            ):
                try:
                    geometry = CONTRACT._validate_unique_text_geometry(
                        page[key]
                    )
                except CONTRACT.MalformedReport as error:
                    raise GeometryContractError(
                        "unique_text_geometry_page"
                    ) from error
                rxls_items = _nonnegative_integer(
                    page.get(f"{prefix}_rxls_items")
                )
                libreoffice_items = _nonnegative_integer(
                    page.get(f"{prefix}_libreoffice_items")
                )
                paired_items = _nonnegative_integer(
                    page.get(f"{prefix}_matched_items")
                )
                if (
                    geometry["rxls_unique_items"] > rxls_items
                    or geometry["libreoffice_unique_items"]
                    > libreoffice_items
                    or geometry["matched_items"] > paired_items
                ):
                    raise GeometryContractError(
                        "unique_text_geometry_count"
                    )
                histogram_buckets += sum(
                    len(
                        geometry["delta_histograms_millipoints"][
                            axis
                        ]
                    )
                    for axis in CONTRACT.UNIQUE_TEXT_GEOMETRY_AXES
                )
            if (
                geometry_pages > max_pages
                or histogram_buckets > max_histogram_buckets
            ):
                raise GeometryContractError(
                    "unique_text_geometry_report_limit"
                )
    return geometry_pages, histogram_buckets


def validate_report_rows(
    files: Sequence[dict[str, Any]],
    *,
    max_pages: int | None = None,
    max_histogram_buckets: int | None = None,
) -> tuple[int, int]:
    """Apply the complete repeatability row contract to a report fragment."""
    max_pages, max_histogram_buckets = _report_limits(
        max_pages,
        max_histogram_buckets,
    )
    seen: set[str] = set()
    geometry_pages = 0
    histogram_buckets = 0
    try:
        for row in files:
            if not isinstance(row, dict):
                raise CONTRACT.MalformedReport("file_row")
            digest = CONTRACT._sha256(
                row.get("sha256"),
                "input_sha256",
            )
            if digest in seen:
                raise CONTRACT.MalformedReport("overlapping_input")
            seen.add(digest)
            CONTRACT._text(row.get("path"), "input_path")
            CONTRACT._integer(row.get("bytes"), "input_bytes")
            CONTRACT._text(
                row.get("format"),
                "input_format",
                maximum=32,
            )
            features = row.get("features", [])
            if (
                not isinstance(features, list)
                or len(features) > 256
                or not all(
                    isinstance(feature, str) and feature
                    for feature in features
                )
                or features != sorted(set(features))
                or row.get("rights_tier")
                not in {None, "S", "U", "Q"}
            ):
                raise CONTRACT.MalformedReport(
                    "baseline_input_identity"
                )
            status = CONTRACT._text(
                row.get("status"),
                "file_status",
                maximum=128,
            )
            classification = CONTRACT._text(
                row.get("classification"),
                "file_classification",
                maximum=256,
            )
            if (
                status not in CONTRACT.REPORT_STATUSES
                or CONTRACT.CLASSIFICATION_RE.fullmatch(
                    classification
                )
                is None
            ):
                raise CONTRACT.MalformedReport(
                    "file_status_or_classification"
                )
            if status in {"compared", "different"}:
                row_pages, row_buckets = (
                    CONTRACT._validate_comparable_row(row)
                )
                geometry_pages += row_pages
                histogram_buckets += row_buckets
                if (
                    geometry_pages > max_pages
                    or histogram_buckets > max_histogram_buckets
                ):
                    raise CONTRACT.MalformedReport(
                        "unique_text_geometry_report_limit"
                    )
            elif "metrics" in row or "pages" in row:
                raise CONTRACT.MalformedReport(
                    "incomparable_row_metrics"
                )
    except CONTRACT.MalformedReport as error:
        raise GeometryContractError(str(error)) from error
    return geometry_pages, histogram_buckets
