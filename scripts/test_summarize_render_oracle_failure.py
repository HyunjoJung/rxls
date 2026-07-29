#!/usr/bin/env python3
"""Tests for sanitized Render Oracle failure diagnostics."""

from __future__ import annotations

from collections import Counter
import copy
from contextlib import redirect_stderr
from fractions import Fraction
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "summarize-render-oracle-failure.py"
HEAD_SHA = "a" * 40


def _load():
    spec = importlib.util.spec_from_file_location(
        "summarize_render_oracle_failure", SCRIPT
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MODULE = _load()


def _row(
    index: int,
    *,
    classification: str = "within_threshold",
    features: tuple[str, ...] = ("latin-text", "number-cell"),
    format_name: str = "xlsx",
    status: str = "compared",
) -> dict[str, object]:
    return {
        "classification": classification,
        "commands": {
            "libreoffice": {
                "stderr": "private workbook content",
            }
        },
        "features": list(sorted(features)),
        "format": format_name,
        "path": f"/srv/private/customer-{index}.xlsx",
        "sha256": hashlib.sha256(f"case-{index}".encode()).hexdigest(),
        "status": status,
    }


def _point_text(value: Fraction) -> str:
    return f"{value.numerator}/{value.denominator}"


def _geometry_page(
    *,
    crop_width_delta: Fraction = Fraction(),
    xhtml_internal_width_delta: Fraction = Fraction(),
) -> dict[str, object]:
    width = Fraction(600)
    height = Fraction(450)

    def side(
        *,
        crop_width: Fraction = width,
    ) -> dict[str, object]:
        def dimensions(
            item_width: Fraction, item_height: Fraction
        ) -> dict[str, str]:
            return {
                "height_points": _point_text(item_height),
                "width_points": _point_text(item_width),
            }

        return {
            "crop_box": dimensions(crop_width, height),
            "media_box": dimensions(width, height),
            "page_size": dimensions(width, height),
        }

    libreoffice = side()
    rxls = side(crop_width=width + crop_width_delta)
    xhtml_width = width + xhtml_internal_width_delta
    xhtml = {
        name: {
            "height_points": _point_text(height),
            "width_points": _point_text(xhtml_width),
        }
        for name in ("libreoffice", "rxls")
    }
    deltas = {
        "crop_box_height": Fraction(),
        "crop_box_width": crop_width_delta,
        "libreoffice_xhtml_page_size_height": Fraction(),
        "libreoffice_xhtml_page_size_width": xhtml_internal_width_delta,
        "media_box_height": Fraction(),
        "media_box_width": Fraction(),
        "rxls_xhtml_page_size_height": Fraction(),
        "rxls_xhtml_page_size_width": xhtml_internal_width_delta,
        "xhtml_height": Fraction(),
        "xhtml_width": Fraction(),
    }
    return {
        "pdf_point_geometry": {
            "deltas_points": {
                key: _point_text(value)
                for key, value in sorted(deltas.items())
            },
            "libreoffice": libreoffice,
            "rxls": rxls,
            "xhtml": xhtml,
        }
    }


def _ceil_scaled(value: Fraction, scale: int) -> int:
    absolute = abs(value)
    return (
        absolute.numerator * scale + absolute.denominator - 1
    ) // absolute.denominator


def _with_geometry(
    row: dict[str, object],
    pages: list[dict[str, object]],
) -> dict[str, object]:
    direct = MODULE.PDF_DIRECT_POINT_DELTA_KEYS
    crosscheck = MODULE.PDF_XHTML_CROSSCHECK_DELTA_KEYS
    parsed = [
        {
            key: Fraction(value)
            for key, value in page["pdf_point_geometry"][
                "deltas_points"
            ].items()
        }
        for page in pages
    ]
    mismatch_pages = sum(
        any(values[key] != 0 for key in direct)
        or any(
            abs(values[key]) > Fraction(1, 1000)
            for key in crosscheck
        )
        for values in parsed
    )
    direct_max = max(
        (
            abs(values[key])
            for values in parsed
            for key in direct
        ),
        default=Fraction(),
    )
    crosscheck_max = max(
        (
            abs(values[key])
            for values in parsed
            for key in crosscheck
        ),
        default=Fraction(),
    )
    for page_offset, page in enumerate(pages):
        page["oracle_output_page_index"] = page_offset
    row["pages"] = pages
    row["metrics"] = {
        "pages": len(pages),
        "max_pdf_point_geometry_delta_millipoints": _ceil_scaled(
            direct_max, 1000
        ),
        "max_pdf_xhtml_crosscheck_delta_micropoints": _ceil_scaled(
            crosscheck_max, 1_000_000
        ),
        "pdf_point_geometry_mismatches": mismatch_pages,
    }
    return row


def _lane_limit(profile: str, label: str) -> int:
    return MODULE.LANES[profile][label]


def _report(
    rows: list[dict[str, object]],
    *,
    profile: str,
    label: str,
    shard_index: int | None = None,
    identity: str = "stable",
) -> dict[str, object]:
    statuses = Counter(str(row["status"]) for row in rows)
    classifications = Counter(str(row["classification"]) for row in rows)
    lane_limit = _lane_limit(profile, label)
    return {
        "configuration": {"identity": identity},
        "discovery": {
            "candidate_count": MODULE.CASES[profile],
            "pre_shard_selected_count": lane_limit,
            "selected_count": len(rows),
            "shard_candidate_count": len(rows),
            "shard_count": 1 if shard_index is None else 4,
            "shard_index": 0 if shard_index is None else shard_index,
            "truncated": False,
        },
        "files": rows,
        "mode": "compare",
        "preflight": {"identity": identity},
        "schema": MODULE.INPUT_SCHEMA,
        "summary": {
            "by_classification": dict(sorted(classifications.items())),
            "by_status": dict(sorted(statuses.items())),
            "files": len(rows),
            "input_bytes_considered": 999,
            "metric_cohorts": {"private": "ignored"},
        },
    }


def _write(path: Path, document: object) -> None:
    path.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def _pilot_rows() -> list[dict[str, object]]:
    formats = ("ods", "xls", "xlsb", "xlsx")
    rows = []
    for index in range(40):
        rows.append(
            _row(
                index,
                format_name=formats[index % len(formats)],
                features=(
                    ("korean-text", "latin-text", "number-cell")
                    if index % 2
                    else ("latin-text", "number-cell")
                ),
            )
        )
    rows[0]["classification"] = "libreoffice_adapter_profile_path_missing"
    rows[0]["status"] = "error"
    return rows


class RenderOracleFailureSummaryTests(unittest.TestCase):
    def test_pilot_summary_is_canonical_and_path_neutral(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            hosted = root / "hosted"
            hosted.mkdir()
            _write(
                hosted / "parity-report-a.json",
                _report(_pilot_rows(), profile="pilot", label="parity-a"),
            )
            authored_rows = [
                _row(
                    1000 + index,
                    features=("latin-text", "number-cell", "print-settings"),
                )
                for index in range(4)
            ]
            for index, (rxls_pages, libreoffice_pages) in enumerate(
                ((4, 3), (2, 3), (4, 3))
            ):
                authored_rows[index].update(
                    {
                        "classification": "page_count_mismatch",
                        "libreoffice_pages": libreoffice_pages,
                        "private_measurement": {
                            "path": f"/srv/private/page-map-{index}.json",
                            "text": "private page content",
                        },
                        "rxls_pages": rxls_pages,
                        "status": "error",
                    }
                )
            _write(
                hosted / "authored-print-report.json",
                _report(
                    authored_rows,
                    profile="pilot",
                    label="authored-print",
                ),
            )

            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )
            output = root / MODULE.OUTPUT_NAME
            MODULE.write_atomic(output, summary)
            payload = output.read_bytes()

            self.assertEqual(payload, MODULE._json(json.loads(payload)))
            self.assertLessEqual(len(payload), MODULE.MAX_OUTPUT_BYTES)
            self.assertEqual(
                [row["label"] for row in summary["reports"]],
                ["authored-print", "parity-a", "parity-b"],
            )
            parity = summary["reports"][1]
            self.assertEqual(parity["workbooks"], 40)
            self.assertEqual(
                parity["by_status"], {"compared": 39, "error": 1}
            )
            self.assertEqual(
                parity["by_classification"][
                    "libreoffice_adapter_profile_path_missing"
                ],
                1,
            )
            self.assertEqual(
                summary["schema"],
                "rxls.render-oracle-failure-summary.v5",
            )
            self.assertEqual(parity["by_format"]["xlsx"]["workbooks"], 10)
            self.assertEqual(
                parity["by_feature"]["korean-text"]["workbooks"], 20
            )
            authored = summary["reports"][0]
            self.assertEqual(
                authored["by_classification"],
                {"page_count_mismatch": 3, "within_threshold": 1},
            )
            self.assertEqual(
                authored["page_count_mismatches"],
                [
                    {
                        "libreoffice_pages": 3,
                        "rxls_pages": 2,
                        "workbooks": 1,
                    },
                    {
                        "libreoffice_pages": 3,
                        "rxls_pages": 4,
                        "workbooks": 2,
                    },
                ],
            )
            self.assertEqual(parity["page_count_mismatches"], [])
            self.assertEqual(summary["reports"][2], MODULE._empty("parity-b"))

            rendered = payload.decode("utf-8")
            self.assertNotIn("/srv/private", rendered)
            self.assertNotIn("private workbook content", rendered)
            self.assertNotIn("private page content", rendered)
            self.assertNotIn("private_measurement", rendered)
            self.assertNotIn('"commands"', rendered)
            self.assertNotIn('"path"', rendered)
            self.assertNotIn('"sha256"', rendered)

    def test_geometry_summary_is_aggregate_only_and_boundary_exact(self) -> None:
        rows = _pilot_rows()
        _with_geometry(
            rows[1],
            [
                _geometry_page(
                    xhtml_internal_width_delta=Fraction(1, 1000)
                )
            ],
        )
        _with_geometry(
            rows[2],
            [
                _geometry_page(
                    xhtml_internal_width_delta=Fraction(1001, 1_000_000)
                )
            ],
        )
        _with_geometry(
            rows[3],
            [_geometry_page(crop_width_delta=Fraction(1, 2))],
        )
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        geometry = summary["reports"][1]["geometry"]
        self.assertEqual(geometry["workbooks"], 3)
        self.assertEqual(geometry["pages"], 3)
        self.assertEqual(geometry["mismatch_pages"], 2)
        self.assertEqual(
            geometry["max_direct_absolute_delta_micropoints"],
            500_000,
        )
        self.assertEqual(
            geometry["max_internal_xhtml_crosscheck_micropoints"],
            1001,
        )
        self.assertEqual(
            set(geometry["by_delta"]), set(MODULE.PDF_POINT_DELTA_KEYS)
        )
        self.assertEqual(
            geometry["by_delta"]["crop_box_width"],
            {
                "max_absolute_micropoints": 500_000,
                "nonzero_pages": 1,
            },
        )
        for key in (
            "libreoffice_xhtml_page_size_width",
            "rxls_xhtml_page_size_width",
        ):
            self.assertEqual(
                geometry["by_delta"][key],
                {
                    "max_absolute_micropoints": 1001,
                    "nonzero_pages": 2,
                },
            )
        self.assertEqual(
            geometry["by_delta"]["media_box_height"],
            {
                "max_absolute_micropoints": 0,
                "nonzero_pages": 0,
            },
        )
        rendered = MODULE._json(summary).decode("ascii")
        for forbidden in (
            "600/1",
            "450/1",
            "1001/1000000",
            "pdf_point_geometry",
            "deltas_points",
        ):
            self.assertNotIn(forbidden, rendered)

    def test_geometry_evidence_shapes_are_fail_closed(self) -> None:
        def rows() -> list[dict[str, object]]:
            value = _pilot_rows()
            _with_geometry(value[1], [_geometry_page()])
            return value

        mutations = {
            "point-extra": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ].__setitem__("private_path", "/srv/private/book.xlsx"),
            "delta-extra": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ]["deltas_points"].__setitem__("private_delta", "0/1"),
            "delta-missing": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ]["deltas_points"].pop("crop_box_width"),
            "side-extra": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ]["rxls"].__setitem__("private_box", {}),
            "dimension-extra": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ]["rxls"]["media_box"].__setitem__(
                "private_path", "/srv/private/book.xlsx"
            ),
            "xhtml-extra": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ]["xhtml"].__setitem__("private_side", {}),
            "page-not-object": lambda row: row["pages"].__setitem__(
                0, "/srv/private/book.xlsx"
            ),
            "pages-not-list": lambda row: row.__setitem__(
                "pages", "/srv/private/book.xlsx"
            ),
            "metrics-not-object": lambda row: row.__setitem__(
                "metrics", "/srv/private/book.xlsx"
            ),
            "partial-row": lambda row: row.pop("metrics"),
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                value = rows()
                mutation(value[1])
                _write(
                    hosted / "parity-report-a.json",
                    _report(value, profile="pilot", label="parity-a"),
                )
                with self.assertRaises(MODULE.SummaryError) as raised:
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )
                message = str(raised.exception)
                self.assertNotIn("/srv/private", message)
                self.assertNotIn("book.xlsx", message)

    def test_geometry_deltas_and_row_aggregates_are_recomputed(self) -> None:
        def rows() -> list[dict[str, object]]:
            value = _pilot_rows()
            _with_geometry(
                value[1],
                [_geometry_page(crop_width_delta=Fraction(1, 2))],
            )
            return value

        mutations = {
            "delta-drift": lambda row: row["pages"][0][
                "pdf_point_geometry"
            ]["deltas_points"].__setitem__("crop_box_width", "1/3"),
            "mismatch-drift": lambda row: row["metrics"].__setitem__(
                "pdf_point_geometry_mismatches", 0
            ),
            "direct-max-drift": lambda row: row["metrics"].__setitem__(
                "max_pdf_point_geometry_delta_millipoints", 499
            ),
            "crosscheck-max-drift": lambda row: row["metrics"].__setitem__(
                "max_pdf_xhtml_crosscheck_delta_micropoints", 1
            ),
            "page-count-drift": lambda row: row["metrics"].__setitem__(
                "pages", 2
            ),
            "boolean-aggregate": lambda row: row["metrics"].__setitem__(
                "pdf_point_geometry_mismatches", True
            ),
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                value = rows()
                mutation(value[1])
                _write(
                    hosted / "parity-report-a.json",
                    _report(value, profile="pilot", label="parity-a"),
                )
                with self.assertRaises(MODULE.SummaryError):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

    def test_geometry_pages_are_bound_to_canonical_output_order(self) -> None:
        def rows() -> list[dict[str, object]]:
            value = _pilot_rows()
            _with_geometry(
                value[1],
                [
                    _geometry_page(),
                    _geometry_page(crop_width_delta=Fraction(1, 2)),
                ],
            )
            return value

        mutations = {
            "missing-index": lambda row: row["pages"][0].pop(
                "oracle_output_page_index"
            ),
            "duplicate-index": lambda row: row["pages"][1].__setitem__(
                "oracle_output_page_index", 0
            ),
            "reordered-pages": lambda row: row["pages"].reverse(),
            "boolean-index": lambda row: row["pages"][0].__setitem__(
                "oracle_output_page_index", False
            ),
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                value = rows()
                mutation(value[1])
                _write(
                    hosted / "parity-report-a.json",
                    _report(value, profile="pilot", label="parity-a"),
                )
                with self.assertRaisesRegex(
                    MODULE.SummaryError, "geometry_page_index"
                ):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

    def test_geometry_rationals_are_bounded_canonical_and_path_neutral(
        self,
    ) -> None:
        def rows() -> list[dict[str, object]]:
            value = _pilot_rows()
            _with_geometry(value[1], [_geometry_page()])
            return value

        replacements = {
            "numerator-limit": (
                "9" * (MODULE.MAX_POINT_RATIONAL_DIGITS + 1) + "/1"
            ),
            "denominator-limit": (
                "1/" + "9" * (MODULE.MAX_POINT_RATIONAL_DIGITS + 1)
            ),
            "noncanonical": "2/2",
            "zero-dimension": "0/1",
            "negative-dimension": "-1/1",
            "dimension-limit": (
                f"{MODULE.MAX_POINT_ABSOLUTE_VALUE + 1}/1"
            ),
            "secret": "/srv/private/customer-payroll.xlsx",
        }
        for label, replacement in replacements.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                value = rows()
                value[1]["pages"][0]["pdf_point_geometry"]["rxls"][
                    "crop_box"
                ]["width_points"] = replacement
                _write(
                    hosted / "parity-report-a.json",
                    _report(value, profile="pilot", label="parity-a"),
                )
                with self.assertRaises(MODULE.SummaryError) as raised:
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )
                message = str(raised.exception)
                self.assertNotIn("/srv/private", message)
                self.assertNotIn("customer-payroll", message)

    def test_geometry_summary_is_invariant_to_order_and_private_fields(
        self,
    ) -> None:
        rows_a = _pilot_rows()
        _with_geometry(
            rows_a[1],
            [_geometry_page(crop_width_delta=Fraction(1, 2))],
        )
        _with_geometry(
            rows_a[2],
            [
                _geometry_page(
                    xhtml_internal_width_delta=Fraction(1, 1000)
                )
            ],
        )
        rows_b = copy.deepcopy(rows_a)
        rows_b.reverse()
        for index, row in enumerate(rows_b):
            row["path"] = f"/private/tenant/secret-{index}.xlsx"
            row["sha256"] = hashlib.sha256(
                f"replacement-{index}".encode()
            ).hexdigest()
            row["commands"] = {
                "stderr": f"private workbook content {index}"
            }
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            first = root / "first"
            second = root / "second"
            first.mkdir()
            second.mkdir()
            _write(
                first / "parity-report-a.json",
                _report(rows_a, profile="pilot", label="parity-a"),
            )
            _write(
                second / "parity-report-a.json",
                _report(rows_b, profile="pilot", label="parity-a"),
            )
            summary_a = MODULE.summarize(
                first,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )
            summary_b = MODULE.summarize(
                second,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        self.assertEqual(summary_a, summary_b)
        rendered = MODULE._json(summary_b).decode("ascii")
        for forbidden in (
            "/private/tenant",
            "private workbook content",
            "replacement-",
            '"sha256"',
            '"path"',
        ):
            self.assertNotIn(forbidden, rendered)

    def test_missing_reports_emit_only_fixed_empty_labels(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "missing"
            summary = MODULE.summarize(
                root,
                profile="full",
                baseline_mode="candidate",
                head_sha=HEAD_SHA,
            )

        self.assertEqual(
            summary,
            {
                "baseline_mode": "candidate",
                "head_sha": HEAD_SHA,
                "profile": "full",
                "reports": [
                    MODULE._empty("authored-print"),
                    MODULE._empty("parity-a"),
                    MODULE._empty("parity-b"),
                ],
                "schema": MODULE.OUTPUT_SCHEMA,
            },
        )

    def test_ooxml_row_diagnostic_summary_is_fixed_and_content_neutral(self) -> None:
        rows = []
        for index in range(12):
            features = {
                "normal-font-noto" if index < 8 else "normal-font-carlito",
                "normal-size-11" if index < 8 else "normal-size-12",
                "ooxml-implicit-row",
                "sheet-format-missing" if index < 8 else "sheet-format-present",
            }
            if index == 4:
                features.add("explicit-row-height")
            rows.append(
                _row(
                    index,
                    classification=(
                        "pdfinfo_page_size_invalid"
                        if index == 0
                        else "within_threshold"
                    ),
                    features=tuple(features),
                    format_name="xlsx",
                    status="error" if index == 0 else "compared",
                )
            )
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(
                    rows,
                    profile="ooxml-row-diagnostic",
                    label="parity-a",
                ),
            )
            summary = MODULE.summarize(
                hosted,
                profile="ooxml-row-diagnostic",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        self.assertEqual(summary["schema"], MODULE.OUTPUT_SCHEMA)
        self.assertEqual(summary["profile"], "ooxml-row-diagnostic")
        self.assertEqual(summary["reports"][1]["workbooks"], 12)
        self.assertEqual(
            summary["reports"][1]["by_classification"],
            {"measurement_geometry_stage": 1, "within_threshold": 11},
        )
        self.assertEqual(summary["reports"][0], MODULE._empty("authored-print"))
        self.assertEqual(summary["reports"][2], MODULE._empty("parity-b"))
        rendered = MODULE._json(summary).decode("ascii")
        self.assertNotIn("/srv/private", rendered)
        self.assertNotIn("private workbook content", rendered)

        with self.assertRaisesRegex(MODULE.SummaryError, "invocation"):
            MODULE.summarize(
                Path("/does-not-matter"),
                profile="ooxml-row-diagnostic",
                baseline_mode="candidate",
                head_sha=HEAD_SHA,
            )

    def test_partial_full_shards_are_aggregated_without_input_identity(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            first = [
                _row(
                    index,
                    classification="libreoffice_adapter_profile_setup_failed",
                    format_name="xls",
                    status="error",
                )
                for index in range(3)
            ]
            second = [
                _row(
                    100 + index,
                    classification="renderer_failed",
                    format_name="ods",
                    status="error",
                )
                for index in range(2)
            ]
            _write(
                hosted / "parity-a-shard-0.json",
                _report(
                    first,
                    profile="full",
                    label="parity-a",
                    shard_index=0,
                ),
            )
            _write(
                hosted / "parity-a-shard-1.json",
                _report(
                    second,
                    profile="full",
                    label="parity-a",
                    shard_index=1,
                ),
            )

            summary = MODULE.summarize(
                hosted,
                profile="full",
                baseline_mode="candidate",
                head_sha=HEAD_SHA,
            )

        parity = summary["reports"][1]
        self.assertEqual(parity["workbooks"], 5)
        self.assertEqual(parity["by_status"], {"error": 5})
        self.assertEqual(
            parity["by_format"],
            {
                "ods": {
                    "by_classification": {"renderer_failed": 2},
                    "workbooks": 2,
                },
                "xls": {
                    "by_classification": {
                        "libreoffice_adapter_profile_setup_failed": 3
                    },
                    "workbooks": 3,
                },
            },
        )

    def test_reported_counts_must_match_rows(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            document = _report(
                _pilot_rows(), profile="pilot", label="parity-a"
            )
            document["summary"]["by_status"] = {"compared": 40}
            _write(hosted / "parity-report-a.json", document)

            with self.assertRaisesRegex(MODULE.SummaryError, "summary_status"):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

    def test_page_count_mismatch_input_is_bounded_and_fail_closed(self) -> None:
        def valid_rows() -> list[dict[str, object]]:
            rows = _pilot_rows()
            rows[1].update(
                {
                    "classification": "page_count_mismatch",
                    "libreoffice_pages": 3,
                    "rxls_pages": 4,
                    "status": "error",
                }
            )
            return rows

        mutations = {
            "missing-rxls": lambda row: row.pop("rxls_pages"),
            "missing-libreoffice": lambda row: row.pop("libreoffice_pages"),
            "negative": lambda row: row.__setitem__("rxls_pages", -1),
            "zero": lambda row: row.__setitem__("libreoffice_pages", 0),
            "oversized": lambda row: row.__setitem__(
                "rxls_pages", MODULE.MAX_PAGE_COUNT + 1
            ),
            "boolean": lambda row: row.__setitem__("rxls_pages", True),
            "injected-string": lambda row: row.__setitem__(
                "rxls_pages", "/srv/private/customer.xlsx"
            ),
            "injected-object": lambda row: row.__setitem__(
                "libreoffice_pages",
                {"path": "/srv/private/customer.xlsx", "text": "secret"},
            ),
            "equal-counts": lambda row: row.__setitem__("rxls_pages", 3),
            "wrong-status": lambda row: row.__setitem__("status", "compared"),
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                rows = valid_rows()
                mutation(rows[1])
                _write(
                    hosted / "parity-report-a.json",
                    _report(rows, profile="pilot", label="parity-a"),
                )
                with self.assertRaisesRegex(
                    MODULE.SummaryError, r"\Apage_count_diagnostic\Z"
                ) as raised:
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )
                message = str(raised.exception)
                self.assertNotIn("/srv/private", message)
                self.assertNotIn("secret", message)

    def test_schema_and_discovery_are_fail_closed(self) -> None:
        for mutation, code in (
            (lambda value: value.__setitem__("schema", "unreviewed"), "report_schema"),
            (
                lambda value: value["discovery"].__setitem__("candidate_count", 39),
                "discovery_coverage",
            ),
            (
                lambda value: value["discovery"].__setitem__("truncated", True),
                "discovery_coverage",
            ),
        ):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                document = _report(
                    _pilot_rows(), profile="pilot", label="parity-a"
                )
                mutation(document)
                _write(hosted / "parity-report-a.json", document)
                with self.assertRaisesRegex(MODULE.SummaryError, code):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

    def test_hostile_json_is_rejected_with_one_path_neutral_cli_error(self) -> None:
        document = _report(
            _pilot_rows(), profile="pilot", label="parity-a"
        )
        canonical = json.dumps(document, sort_keys=True)
        hostile_payloads = {
            "duplicate-key": canonical.replace(
                '{"configuration":',
                (
                    '{"schema":"rxls.libreoffice-render-parity.v1",'
                    '"configuration":'
                ),
                1,
            ),
            "non-finite": canonical.replace('"stable"', "NaN", 1),
            "decimal": canonical.replace('"stable"', "1.5", 1),
            "exponent": canonical.replace('"stable"', "1e10000", 1),
            "integer-limit": canonical.replace(
                '"stable"', "9" * (MODULE.MAX_JSON_INTEGER_DIGITS + 1), 1
            ),
            "depth-limit": canonical.replace(
                '"stable"',
                (
                    "[" * (MODULE.MAX_JSON_DEPTH + 1)
                    + "0"
                    + "]" * (MODULE.MAX_JSON_DEPTH + 1)
                ),
                1,
            ),
        }
        for label, payload in hostile_payloads.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                hosted = root / "hosted"
                hosted.mkdir()
                report = hosted / "parity-report-a.json"
                report.write_text(payload, encoding="utf-8")
                output = root / MODULE.OUTPUT_NAME
                stderr = io.StringIO()
                with redirect_stderr(stderr):
                    result = MODULE.main(
                        (
                            "--input-root",
                            str(hosted),
                            "--profile",
                            "pilot",
                            "--baseline-mode",
                            "verify",
                            "--head-sha",
                            HEAD_SHA,
                            "--output",
                            str(output),
                        )
                    )
                self.assertEqual(result, 1)
                self.assertEqual(
                    stderr.getvalue(),
                    "render-oracle-failure-summary: report_unreadable\n",
                )
                self.assertFalse(output.exists())
                self.assertNotIn(str(root), stderr.getvalue())

    def test_classification_format_and_feature_are_bounded(self) -> None:
        mutations = (
            ("classification", "private/customer.xlsx"),
            ("format", "private"),
            ("features", ["latin-text", "private-customer-name"]),
        )
        for field, replacement in mutations:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                rows = _pilot_rows()
                rows[0][field] = replacement
                document = _report(rows, profile="pilot", label="parity-a")
                _write(hosted / "parity-report-a.json", document)
                with self.assertRaises(MODULE.SummaryError):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

    def test_unreviewed_snake_case_classifications_are_bucketed(self) -> None:
        secret_codes = (
            "source_path_sha256",
            "host_path_digest",
            "srv_private_customer_path_digest",
        )
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            rows = _pilot_rows()
            for index, code in enumerate(secret_codes, start=1):
                rows[index]["classification"] = code
                rows[index]["status"] = "error"
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )

            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        parity = summary["reports"][1]
        self.assertEqual(
            parity["by_classification"][
                MODULE.UNREVIEWED_CLASSIFICATION
            ],
            len(secret_codes),
        )
        rendered = MODULE._json(summary).decode("ascii")
        for secret_code in secret_codes:
            self.assertNotIn(secret_code, rendered)
        self.assertEqual(
            set(parity["by_classification"])
            - MODULE.OUTPUT_CLASSIFICATIONS,
            set(),
        )

    def test_unknown_details_reduce_to_allowlisted_coarse_stages(self) -> None:
        exact_codes = {
            "renderer_pdf_type3_path_text_missing": (
                "renderer_pdf_type3_path_text_missing"
            ),
            "libreoffice_font_pack_mismatch": "libreoffice_font_pack_mismatch",
        }
        coarse_codes = {
            "renderer_print_pdf_page_map": "renderer_page_map_stage",
            "renderer_pdf_raster_output_limit": "renderer_raster_stage",
            "renderer_semantic_bbox_unreadable": "renderer_semantic_stage",
            "render_manifest_scene_mismatch": "renderer_bundle_stage",
            "libreoffice_adapter_image_identity": "oracle_adapter_stage",
            "libreoffice_pdf_invalid": "oracle_pdf_stage",
            "libreoffice_page_limit": "oracle_raster_stage",
            "pdfinfo_page_size_invalid": "measurement_geometry_stage",
            "pdf_raster_missing": "measurement_raster_stage",
            "semantic_bbox_output_limit": "measurement_semantic_stage",
            "page_count_mismatch_private_customer": "measurement_stage",
            "authored_print_no_visible_pages": "authored_print_stage",
            "font_pack_required": "environment_stage",
            "manifest_local_path_unsafe": "input_stage",
            "private_customer_path_digest": MODULE.UNREVIEWED_CLASSIFICATION,
            "renderer_private_customer_path_digest": "renderer_stage",
            "renderer_pdf_type3_path_text_missing_private_customer": (
                "renderer_pdf_attestation_stage"
            ),
            "libreoffice_font_pack_mismatch_private_path": (
                "oracle_font_attestation_stage"
            ),
        }
        detailed_codes = {**exact_codes, **coarse_codes}
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            rows = _pilot_rows()
            for index, code in enumerate(detailed_codes, start=1):
                rows[index]["classification"] = code
                rows[index]["status"] = "error"
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )

            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        parity = summary["reports"][1]
        expected = Counter(detailed_codes.values())
        for bucket, count in expected.items():
            self.assertEqual(parity["by_classification"][bucket], count)
            self.assertEqual(
                parity["by_feature"]["latin-text"]["by_classification"][
                    bucket
                ],
                count,
            )
        self.assertEqual(parity["page_count_mismatches"], [])
        rendered = MODULE._json(summary).decode("ascii")
        for code in coarse_codes:
            self.assertNotIn(code, rendered)
        for code in exact_codes:
            self.assertIn(code, rendered)
        for forbidden in (
            "private_customer",
            "path_digest",
            '"commands"',
            '"path"',
            '"stderr"',
            '"stdout"',
        ):
            self.assertNotIn(forbidden, rendered)
        self.assertEqual(
            set(parity["by_classification"])
            - MODULE.OUTPUT_CLASSIFICATIONS,
            set(),
        )

    def test_merged_and_sharded_inputs_cannot_be_mixed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            rows = [_row(index) for index in range(800)]
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="full", label="parity-a"),
            )
            _write(
                hosted / "parity-a-shard-0.json",
                _report(
                    rows[:200],
                    profile="full",
                    label="parity-a",
                    shard_index=0,
                ),
            )
            with self.assertRaisesRegex(
                MODULE.SummaryError, "report_fragment_ambiguity"
            ):
                MODULE.summarize(
                    hosted,
                    profile="full",
                    baseline_mode="candidate",
                    head_sha=HEAD_SHA,
                )

    def test_unreviewed_raw_report_name_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(hosted / "parity-report-secret.json", {})
            with self.assertRaisesRegex(
                MODULE.SummaryError, "unexpected_report_name"
            ):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

    def test_duplicate_workbooks_across_shards_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            row = _row(1)
            for index in range(2):
                _write(
                    hosted / f"parity-a-shard-{index}.json",
                    _report(
                        [row],
                        profile="full",
                        label="parity-a",
                        shard_index=index,
                    ),
                )
            with self.assertRaisesRegex(MODULE.SummaryError, "duplicate_workbook"):
                MODULE.summarize(
                    hosted,
                    profile="full",
                    baseline_mode="candidate",
                    head_sha=HEAD_SHA,
                )

    def test_input_and_output_types_and_sizes_are_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            hosted = root / "hosted"
            hosted.mkdir()
            report = hosted / "parity-report-a.json"
            _write(
                report,
                _report(_pilot_rows(), profile="pilot", label="parity-a"),
            )
            with mock.patch.object(MODULE, "MAX_REPORT_BYTES", 1):
                with self.assertRaisesRegex(
                    MODULE.SummaryError, "report_type_or_size"
                ):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

            with mock.patch.object(MODULE, "MAX_OUTPUT_BYTES", 1):
                with self.assertRaisesRegex(
                    MODULE.SummaryError, "output_size"
                ):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

            symlinked = root / "symlinked"
            symlinked.mkdir()
            (symlinked / "parity-report-a.json").symlink_to(report)
            with self.assertRaisesRegex(
                MODULE.SummaryError, "report_type_or_size"
            ):
                MODULE.summarize(
                    symlinked,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

            output = root / MODULE.OUTPUT_NAME
            target = root / "actual.json"
            target.write_text("{}\n", encoding="utf-8")
            output.symlink_to(target)
            with self.assertRaisesRegex(MODULE.SummaryError, "output_type"):
                MODULE.write_atomic(output, {"schema": MODULE.OUTPUT_SCHEMA})

    def test_output_contract_rejects_injected_fields_and_count_drift(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(_pilot_rows(), profile="pilot", label="parity-a"),
            )
            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )
        injected = copy.deepcopy(summary)
        injected["reports"][1]["path"] = "/private/workbook.xlsx"
        drifted = copy.deepcopy(summary)
        drifted["reports"][1]["by_status"]["compared"] = 38
        unreviewed_stage = copy.deepcopy(summary)
        unreviewed_stage["reports"][1]["by_classification"][
            "private_customer_stage"
        ] = 1
        format_conflict = copy.deepcopy(summary)
        ods_classes = format_conflict["reports"][1]["by_format"]["ods"][
            "by_classification"
        ]
        ods_classes.pop("libreoffice_adapter_profile_path_missing")
        ods_classes[MODULE.UNREVIEWED_CLASSIFICATION] = 1
        feature_conflict = copy.deepcopy(summary)
        latin_classes = feature_conflict["reports"][1]["by_feature"][
            "latin-text"
        ]["by_classification"]
        latin_classes["within_threshold"] -= 1
        latin_classes[MODULE.UNREVIEWED_CLASSIFICATION] = 1
        geometry_injected = copy.deepcopy(summary)
        geometry_injected["reports"][1]["geometry"]["private_path"] = (
            "/private/workbook.xlsx"
        )
        delta_injected = copy.deepcopy(summary)
        delta_injected["reports"][1]["geometry"]["by_delta"][
            "private_delta"
        ] = {
            "max_absolute_micropoints": 0,
            "nonzero_pages": 0,
        }
        geometry_drift = copy.deepcopy(summary)
        geometry_drift["reports"][1]["geometry"][
            "max_direct_absolute_delta_micropoints"
        ] = 1
        for document in (
            injected,
            drifted,
            unreviewed_stage,
            format_conflict,
            feature_conflict,
            geometry_injected,
            delta_injected,
            geometry_drift,
        ):
            with self.subTest(document=document):
                with self.assertRaises(MODULE.SummaryError):
                    MODULE._validate_output(document)

    def test_output_page_count_diagnostic_is_bounded_and_consistent(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            rows = _pilot_rows()
            rows[1].update(
                {
                    "classification": "page_count_mismatch",
                    "libreoffice_pages": 3,
                    "rxls_pages": 4,
                    "status": "error",
                }
            )
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        MODULE._validate_output(summary)
        parity = summary["reports"][1]
        self.assertEqual(
            parity["page_count_mismatches"],
            [
                {
                    "libreoffice_pages": 3,
                    "rxls_pages": 4,
                    "workbooks": 1,
                }
            ],
        )

        missing = copy.deepcopy(summary)
        missing["reports"][1].pop("page_count_mismatches")
        negative = copy.deepcopy(summary)
        negative["reports"][1]["page_count_mismatches"][0][
            "rxls_pages"
        ] = -1
        oversized = copy.deepcopy(summary)
        oversized["reports"][1]["page_count_mismatches"][0][
            "libreoffice_pages"
        ] = MODULE.MAX_PAGE_COUNT + 1
        injected_value = copy.deepcopy(summary)
        injected_value["reports"][1]["page_count_mismatches"][0][
            "rxls_pages"
        ] = "/srv/private/customer.xlsx"
        injected_key = copy.deepcopy(summary)
        injected_key["reports"][1]["page_count_mismatches"][0][
            "private_path"
        ] = "/srv/private/customer.xlsx"
        count_drift = copy.deepcopy(summary)
        count_drift["reports"][1]["page_count_mismatches"][0][
            "workbooks"
        ] = 2
        equal_counts = copy.deepcopy(summary)
        equal_counts["reports"][1]["page_count_mismatches"][0][
            "rxls_pages"
        ] = 3
        duplicate_pair = copy.deepcopy(summary)
        duplicate_pair["reports"][1]["page_count_mismatches"].append(
            copy.deepcopy(
                duplicate_pair["reports"][1]["page_count_mismatches"][0]
            )
        )
        for label, document in (
            ("missing", missing),
            ("negative", negative),
            ("oversized", oversized),
            ("injected-value", injected_value),
            ("injected-key", injected_key),
            ("count-drift", count_drift),
            ("equal-counts", equal_counts),
            ("duplicate-pair", duplicate_pair),
        ):
            with self.subTest(label=label):
                with self.assertRaises(MODULE.SummaryError) as raised:
                    MODULE._validate_output(document)
                message = str(raised.exception)
                self.assertNotIn("/srv/private", message)
                self.assertNotIn("customer.xlsx", message)

    def test_head_profile_and_baseline_mode_are_validated(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            for profile, baseline_mode, head_sha, code in (
                ("pilot", "candidate", HEAD_SHA, "invocation"),
                ("pilot", "verify", "A" * 40, "invocation"),
            ):
                with self.subTest(code=code):
                    with self.assertRaisesRegex(MODULE.SummaryError, code):
                        MODULE.summarize(
                            root,
                            profile=profile,
                            baseline_mode=baseline_mode,
                            head_sha=head_sha,
                        )

    def test_cli_does_not_leave_output_after_validation_failure(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            output = root / MODULE.OUTPUT_NAME
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                result = MODULE.main(
                    (
                        "--input-root",
                        str(root / "missing"),
                        "--profile",
                        "pilot",
                        "--baseline-mode",
                        "candidate",
                        "--head-sha",
                        HEAD_SHA,
                        "--output",
                        str(output),
                    )
                )
            self.assertEqual(result, 1)
            self.assertEqual(
                stderr.getvalue(),
                "render-oracle-failure-summary: invocation\n",
            )
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
