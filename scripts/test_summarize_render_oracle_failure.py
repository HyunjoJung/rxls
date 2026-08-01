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
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "summarize-render-oracle-failure.py"
HEAD_SHA = "a" * 40
TEST_CASE_ID_KEY = b"\x5a" * 32


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
    row = {
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
    if status in MODULE.METRIC_BEARING_STATUSES:
        _with_geometry(row, [_geometry_page()])
    return row


def _point_text(value: Fraction) -> str:
    return f"{value.numerator}/{value.denominator}"


def _geometry_page(
    *,
    crop_height_delta: Fraction = Fraction(),
    crop_width_delta: Fraction = Fraction(),
    xhtml_cross_document_width_delta: Fraction = Fraction(),
    xhtml_internal_width_delta: Fraction = Fraction(),
) -> dict[str, object]:
    width = Fraction(600)
    height = Fraction(450)

    def side(
        *,
        crop_height: Fraction = height,
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
            "crop_box": dimensions(crop_width, crop_height),
            "media_box": dimensions(width, height),
            "page_size": dimensions(width, height),
        }

    libreoffice = side()
    rxls = side(
        crop_height=height + crop_height_delta,
        crop_width=width + crop_width_delta,
    )
    libreoffice_xhtml_width = width + xhtml_internal_width_delta
    rxls_xhtml_width = (
        libreoffice_xhtml_width + xhtml_cross_document_width_delta
    )
    xhtml = {
        "libreoffice": {
            "height_points": _point_text(height),
            "width_points": _point_text(libreoffice_xhtml_width),
        },
        "rxls": {
            "height_points": _point_text(height),
            "width_points": _point_text(rxls_xhtml_width),
        },
    }
    deltas = {
        "crop_box_height": crop_height_delta,
        "crop_box_width": crop_width_delta,
        "libreoffice_xhtml_page_size_height": Fraction(),
        "libreoffice_xhtml_page_size_width": xhtml_internal_width_delta,
        "media_box_height": Fraction(),
        "media_box_width": Fraction(),
        "rxls_xhtml_page_size_height": Fraction(),
        "rxls_xhtml_page_size_width": (
            xhtml_internal_width_delta
            + xhtml_cross_document_width_delta
        ),
        "xhtml_height": Fraction(),
        "xhtml_width": xhtml_cross_document_width_delta,
    }
    page = {
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
    page["text_box_unique_geometry"] = _unique_text_geometry(())
    page["text_line_box_unique_geometry"] = _unique_text_geometry(())
    for prefix, geometry in (
        ("text_box", page["text_box_unique_geometry"]),
        ("text_line_box", page["text_line_box_unique_geometry"]),
    ):
        page[f"{prefix}_libreoffice_items"] = geometry[
            "libreoffice_unique_items"
        ]
        page[f"{prefix}_matched_items"] = geometry["matched_items"]
        page[f"{prefix}_rxls_items"] = geometry["rxls_unique_items"]
    return page


def _unique_text_geometry(
    histogram: tuple[tuple[int, int], ...],
    *,
    rxls_unique_items: int | None = None,
    libreoffice_unique_items: int | None = None,
) -> dict[str, object]:
    matched = sum(count for _, count in histogram)
    exact_summary = {
        "count": matched,
        "max_delta_millipoints": histogram[-1][0] if histogram else None,
        "min_delta_millipoints": histogram[0][0] if histogram else None,
        "negative_overflow_items": sum(
            count
            for delta, count in histogram
            if delta < -MODULE.TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS
        ),
        "positive_overflow_items": sum(
            count
            for delta, count in histogram
            if delta > MODULE.TEXT_GEOMETRY_OUTER_LIMIT_MILLIPOINTS
        ),
        "sum_delta_millipoints": sum(
            delta * count for delta, count in histogram
        ),
    }
    return {
        "delta_histograms_millipoints": {
            axis: [
                {"count": count, "delta_millipoints": delta}
                for delta, count in histogram
            ]
            for axis in MODULE.TEXT_GEOMETRY_AXES
        },
        "exact_delta_summaries_millipoints": {
            axis: copy.deepcopy(exact_summary)
            for axis in MODULE.TEXT_GEOMETRY_AXES
        },
        "libreoffice_unique_items": (
            matched + 2
            if libreoffice_unique_items is None
            else libreoffice_unique_items
        ),
        "matched_items": matched,
        "rxls_unique_items": (
            matched + 1
            if rxls_unique_items is None
            else rxls_unique_items
        ),
    }


def _with_unique_text_geometry(
    page: dict[str, object],
    *,
    word_histogram: tuple[tuple[int, int], ...],
    line_histogram: tuple[tuple[int, int], ...],
) -> dict[str, object]:
    page["text_box_unique_geometry"] = _unique_text_geometry(
        word_histogram
    )
    page["text_line_box_unique_geometry"] = _unique_text_geometry(
        line_histogram
    )
    for prefix, geometry in (
        ("text_box", page["text_box_unique_geometry"]),
        ("text_line_box", page["text_line_box_unique_geometry"]),
    ):
        page[f"{prefix}_libreoffice_items"] = geometry[
            "libreoffice_unique_items"
        ]
        page[f"{prefix}_matched_items"] = geometry["matched_items"]
        page[f"{prefix}_rxls_items"] = geometry["rxls_unique_items"]
    return page


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
    semantic_rxls = len(pages) * 10
    semantic_libreoffice = len(pages) * 11
    semantic_matched = len(pages) * 9

    def ratio_fields(
        prefix: str,
        rxls_items: int,
        libreoffice_items: int,
        matched_items: int,
    ) -> dict[str, int]:
        evidence = MODULE._ratio_evidence(
            rxls_items, libreoffice_items, matched_items
        )
        return {
            f"{prefix}_{key}": value
            for key, value in evidence.items()
        }

    def text_fields(prefix: str) -> dict[str, int]:
        rxls_items = sum(
            int(page[f"{prefix}_rxls_items"]) for page in pages
        )
        libreoffice_items = sum(
            int(page[f"{prefix}_libreoffice_items"])
            for page in pages
        )
        matched_items = sum(
            int(page[f"{prefix}_matched_items"]) for page in pages
        )
        evidence = MODULE._text_evidence(
            rxls_items,
            libreoffice_items,
            matched_items,
            0,
            rxls_items - matched_items,
            libreoffice_items - matched_items,
        )
        result = {
            f"{prefix}_{key}": value
            for key, value in evidence.items()
        }
        result[f"{prefix}_candidate_items"] = rxls_items
        result[f"{prefix}_unmatched_items"] = evidence[
            "rxls_unmatched_items"
        ]
        result[f"{prefix}_match_coverage_ppm"] = evidence[
            "precision_ppm"
        ]
        return result

    def mask_fields(prefix: str) -> dict[str, int]:
        evidence = MODULE._mask_evidence(0, 0, 0, 0)
        result = {}
        for key, value in evidence.items():
            suffix = (
                key.replace("_matched_pixels", "_matched_1px")
                if key
                in {
                    "rxls_matched_pixels",
                    "libreoffice_matched_pixels",
                }
                else key
            )
            result[f"{prefix}_{suffix}"] = value
        return result

    pixels = len(pages) * 100
    row["pages"] = pages
    row["metrics"] = {
        "absolute_error_sum": 0,
        "blurred_luma_absolute_error_sum": 0,
        "blurred_luma_similarity_ppm": 1_000_000,
        "changed_pixels": 0,
        "exact_pages": len(pages),
        "max_channel_delta": 0,
        "mean_absolute_error_ppm": 0,
        "mismatch_ppm": 0,
        "pages": len(pages),
        "pixels": pixels,
        "similarity_ppm": 1_000_000,
        "max_pdf_point_geometry_delta_millipoints": _ceil_scaled(
            direct_max, 1000
        ),
        "max_pdf_xhtml_crosscheck_delta_micropoints": _ceil_scaled(
            crosscheck_max, 1_000_000
        ),
        "pdf_point_geometry_mismatches": mismatch_pages,
        **ratio_fields(
            "semantic_codepoint",
            semantic_rxls,
            semantic_libreoffice,
            semantic_matched,
        ),
        **text_fields("text_box"),
        **text_fields("text_line_box"),
        **mask_fields("edge"),
        **mask_fields("foreground"),
        **mask_fields("text_ink"),
    }
    return row


def _as_premeasurement_error(
    row: dict[str, object],
) -> dict[str, object]:
    row["classification"] = "libreoffice_timeout"
    row["status"] = "error"
    row.pop("metrics", None)
    row.pop("pages", None)
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
        "configuration": {
            "identity": identity,
            "metric_policy": {
                "unique_text_geometry": copy.deepcopy(
                    MODULE.TEXT_GEOMETRY_POLICY
                )
            },
        },
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


def _summarize_pilot(
    rows: list[dict[str, object]],
) -> dict[str, object]:
    with tempfile.TemporaryDirectory() as raw:
        hosted = Path(raw)
        _write(
            hosted / "parity-report-a.json",
            _report(rows, profile="pilot", label="parity-a"),
        )
        return MODULE.summarize(
            hosted,
            profile="pilot",
            baseline_mode="verify",
            head_sha=HEAD_SHA,
            _case_id_key_for_test=TEST_CASE_ID_KEY,
        )


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
                _case_id_key_for_test=TEST_CASE_ID_KEY,
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
                "rxls.render-oracle-failure-summary.v10",
            )
            self.assertEqual(
                summary["ingestion"],
                {
                    "expected_workbooks": 44,
                    "received_workbooks": 44,
                    "status": "complete",
                },
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
            self.assertNotIn('"sha256":', rendered)
            self.assertNotIn(TEST_CASE_ID_KEY.hex(), rendered)
            for row in _pilot_rows():
                self.assertNotIn(str(row.get("sha256")), rendered)

    def test_fidelity_cohorts_and_case_ids_are_numeric_and_opaque(
        self,
    ) -> None:
        rows = _pilot_rows()
        summary = _summarize_pilot(rows)
        parity = summary["reports"][1]
        fidelity = parity["fidelity"]

        self.assertEqual(fidelity["all"]["workbooks"], 39)
        self.assertEqual(fidelity["all"]["pages"], 39)
        self.assertEqual(
            fidelity["all"]["semantic_visible_characters"],
            {
                "f1_ppm": 857143,
                "libreoffice_items": 429,
                "matched_items": 351,
                "precision_ppm": 900000,
                "recall_ppm": 818182,
                "rxls_items": 390,
            },
        )
        self.assertEqual(
            fidelity["all"]["poppler_words"]["rxls_items"], 39
        )
        self.assertEqual(
            fidelity["all"]["poppler_words"]["libreoffice_items"], 78
        )
        self.assertEqual(
            fidelity["all"]["poppler_words"]["f1_ppm"], 0
        )
        self.assertEqual(
            fidelity["all"]["poppler_lines"],
            fidelity["all"]["poppler_words"],
        )
        self.assertEqual(
            fidelity["all"]["raster"]["similarity_ppm"], 1_000_000
        )
        self.assertEqual(
            fidelity["all"]["raster"]["pixels"], 3900
        )
        self.assertEqual(
            fidelity["by_format"]["ods"]["workbooks"], 9
        )
        self.assertEqual(
            fidelity["by_format"]["xls"]["workbooks"], 10
        )

        diagnostics = parity["case_diagnostics"]
        self.assertEqual(diagnostics["available_cases"], 39)
        self.assertEqual(diagnostics["retained_cases"], 39)
        self.assertFalse(diagnostics["truncated"])
        self.assertEqual(
            diagnostics["available_cases_by_format"],
            {"ods": 9, "xls": 10, "xlsb": 10, "xlsx": 10},
        )
        self.assertEqual(
            diagnostics["retained_cases_by_format"],
            diagnostics["available_cases_by_format"],
        )
        self.assertTrue(
            all(
                set(case["raster"]) == MODULE.FIDELITY_RASTER_KEYS
                for case in diagnostics["cases"]
            )
        )
        case_ids = [
            case["case_id"] for case in diagnostics["cases"]
        ]
        self.assertEqual(case_ids, sorted(case_ids))
        self.assertEqual(len(case_ids), len(set(case_ids)))
        raw_digests = {
            str(row["sha256"])
            for row in rows
            if row["status"] in MODULE.METRIC_BEARING_STATUSES
        }
        self.assertTrue(raw_digests.isdisjoint(case_ids))
        self.assertTrue(
            all(MODULE.HASH_RE.fullmatch(case_id) for case_id in case_ids)
        )
        self.assertEqual(
            case_ids,
            sorted(
                MODULE._opaque_case_id(
                    str(row["sha256"]), TEST_CASE_ID_KEY
                )
                for row in rows
                if row["status"] in MODULE.METRIC_BEARING_STATUSES
            ),
        )
        alternate_ids = {
            MODULE._opaque_case_id(str(row["sha256"]), b"\xa5" * 32)
            for row in rows
            if row["status"] in MODULE.METRIC_BEARING_STATUSES
        }
        self.assertTrue(alternate_ids.isdisjoint(case_ids))
        self.assertEqual(
            MODULE.CASE_ID_POLICY,
            {
                "algorithm": "hmac-sha256",
                "correlation": "within_summary_only",
                "domain": "rxls.render-oracle-failure-case.v1",
                "input": "domain_separated_workbook_digest",
                "key": "ephemeral_non_exported",
                "max_cases_per_report": 64,
                "selection": "lexicographically_lowest_case_ids",
            },
        )

    def test_default_case_id_key_is_ephemeral_and_test_key_is_strict(
        self,
    ) -> None:
        rows = _pilot_rows()
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            keys = (b"\x11" * 32, b"\x22" * 32)
            with mock.patch.object(
                MODULE.secrets,
                "token_bytes",
                side_effect=keys,
            ) as token_bytes:
                first = MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )
                second = MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

        self.assertEqual(
            token_bytes.call_args_list,
            [mock.call(MODULE.CASE_ID_KEY_BYTES)] * 2,
        )
        first_ids = {
            case["case_id"]
            for case in first["reports"][1]["case_diagnostics"][
                "cases"
            ]
        }
        second_ids = {
            case["case_id"]
            for case in second["reports"][1]["case_diagnostics"][
                "cases"
            ]
        }
        self.assertTrue(first_ids.isdisjoint(second_ids))
        expected_first = {
            MODULE._opaque_case_id(str(row["sha256"]), keys[0])
            for row in rows
            if row["status"] in MODULE.METRIC_BEARING_STATUSES
        }
        self.assertEqual(first_ids, expected_first)
        for key in keys:
            self.assertNotIn(key.hex(), MODULE._json(first).decode())
            self.assertNotIn(key.hex(), MODULE._json(second).decode())
        for invalid in (
            b"",
            b"\x00" * 31,
            b"\x00" * 33,
            bytearray(32),
            "not-bytes",
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaisesRegex(
                    MODULE.SummaryError, "case_id_key"
                ):
                    MODULE._case_id_key(invalid)

    def test_fidelity_and_case_output_contract_is_fail_closed(self) -> None:
        summary = _summarize_pilot(_pilot_rows())
        malformed = []

        value = copy.deepcopy(summary)
        value["reports"][1]["fidelity"]["all"][
            "semantic_visible_characters"
        ]["f1_ppm"] -= 1
        malformed.append(value)

        value = copy.deepcopy(summary)
        value["reports"][1]["case_diagnostics"]["cases"][0][
            "source_url"
        ] = "https://private.invalid/customer.xlsx"
        malformed.append(value)

        value = copy.deepcopy(summary)
        value["reports"][1]["case_diagnostics"]["cases"][0][
            "poppler_words"
        ]["token"] = "private cell contents"
        malformed.append(value)

        value = copy.deepcopy(summary)
        value["reports"][1]["case_diagnostics"][
            "retained_cases"
        ] -= 1
        malformed.append(value)

        value = copy.deepcopy(summary)
        value["reports"][1]["case_diagnostics"]["cases"][0][
            "case_id"
        ] = "a" * 63
        malformed.append(value)

        value = copy.deepcopy(summary)
        value["reports"][1]["case_diagnostics"]["cases"][0][
            "raster"
        ]["similarity_ppm"] = 0
        malformed.append(value)

        value = copy.deepcopy(summary)
        case = value["reports"][1]["case_diagnostics"]["cases"][0]
        case["format"] = (
            "ods" if case["format"] != "ods" else "xlsx"
        )
        malformed.append(value)

        for document in malformed:
            with self.subTest(document=document):
                with self.assertRaises(MODULE.SummaryError):
                    MODULE._validate_output(document)

    def test_raster_raw_page_and_exact_mask_counts_are_feasible(
        self,
    ) -> None:
        accumulator = MODULE._new_fidelity_accumulator()
        accumulator["workbooks"] = 2
        accumulator["pages"] = 2
        accumulator["raster"]["pixels"] = 1
        accumulator["raster"]["exact_pages"] = 2
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_raster"
        ):
            MODULE._validate_raster_output(
                MODULE._finish_fidelity(accumulator)["raster"],
                pages=2,
                code="test_raster",
            )

        accumulator = MODULE._new_fidelity_accumulator()
        accumulator["workbooks"] = 2
        accumulator["pages"] = 2
        accumulator["raster"].update(
            {
                "absolute_error_sum": 2,
                "changed_pixels": 2,
                "exact_pages": 1,
                "max_channel_delta": 1,
                "pixels": 2,
            }
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_raster"
        ):
            MODULE._validate_raster_output(
                MODULE._finish_fidelity(accumulator)["raster"],
                pages=2,
                code="test_raster",
            )

        accumulator = MODULE._new_fidelity_accumulator()
        accumulator["workbooks"] = 2
        accumulator["pages"] = 2
        accumulator["raster"].update(
            {
                "absolute_error_sum": (
                    MODULE.MAX_RASTER_PIXELS_PER_PAGE + 1
                ),
                "changed_pixels": (
                    MODULE.MAX_RASTER_PIXELS_PER_PAGE + 1
                ),
                "exact_pages": 1,
                "max_channel_delta": 1,
                "pixels": MODULE.MAX_RASTER_PIXELS_PER_PAGE + 2,
            }
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_raster"
        ):
            MODULE._validate_raster_output(
                MODULE._finish_fidelity(accumulator)["raster"],
                pages=2,
                code="test_raster",
            )

        accumulator = MODULE._new_fidelity_accumulator()
        accumulator["workbooks"] = 1
        accumulator["pages"] = 1
        accumulator["raster"]["pixels"] = 100
        accumulator["raster"]["exact_pages"] = 1
        raster = MODULE._finish_fidelity(accumulator)["raster"]
        raster["edge"] = MODULE._mask_evidence(1, 0, 0, 0)
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_raster"
        ):
            MODULE._validate_raster_output(
                raster,
                pages=1,
                code="test_raster",
            )

        accumulator = MODULE._new_fidelity_accumulator()
        accumulator["workbooks"] = 2
        accumulator["pages"] = 2
        accumulator["raster"].update(
            {
                "absolute_error_sum": 1,
                "changed_pixels": 1,
                "exact_pages": 1,
                "max_channel_delta": 1,
                "pixels": 2,
            }
        )
        raster = MODULE._finish_fidelity(accumulator)["raster"]
        raster["foreground"] = MODULE._mask_evidence(2, 0, 0, 0)
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_raster"
        ):
            MODULE._validate_raster_output(
                raster,
                pages=2,
                code="test_raster",
            )

        accumulator = MODULE._new_fidelity_accumulator()
        accumulator["workbooks"] = 1
        accumulator["pages"] = 1
        accumulator["raster"].update(
            {
                "absolute_error_sum": 1,
                "blurred_luma_absolute_error_sum": 101,
                "changed_pixels": 1,
                "exact_pages": 0,
                "max_channel_delta": 1,
                "pixels": 100,
            }
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_raster"
        ):
            MODULE._validate_raster_output(
                MODULE._finish_fidelity(accumulator)["raster"],
                pages=1,
                code="test_raster",
            )

    def test_truncated_case_diagnostics_are_strict_aggregate_subsets(
        self,
    ) -> None:
        policy = {
            **MODULE.CASE_ID_POLICY,
            "max_cases_per_report": 2,
        }
        with (
            mock.patch.object(
                MODULE, "MAX_CASE_DIAGNOSTICS_PER_REPORT", 2
            ),
            mock.patch.object(MODULE, "CASE_ID_POLICY", policy),
        ):
            summary = _summarize_pilot(_pilot_rows())
            diagnostics = summary["reports"][1]["case_diagnostics"]
            self.assertTrue(diagnostics["truncated"])
            self.assertEqual(
                diagnostics["available_cases_by_format"],
                {"ods": 9, "xls": 10, "xlsb": 10, "xlsx": 10},
            )
            self.assertEqual(
                sum(diagnostics["retained_cases_by_format"].values()),
                diagnostics["retained_cases"],
            )
            malformed = []

            value = copy.deepcopy(summary)
            semantic = value["reports"][1]["case_diagnostics"][
                "cases"
            ][0]["semantic_visible_characters"]
            semantic.clear()
            semantic.update(MODULE._ratio_evidence(1_000, 1_000, 0))
            malformed.append(value)

            value = copy.deepcopy(summary)
            oversized = MODULE._new_fidelity_accumulator()
            oversized["workbooks"] = 1
            oversized["pages"] = 1
            oversized["raster"]["pixels"] = 10_000
            oversized["raster"]["exact_pages"] = 1
            value["reports"][1]["case_diagnostics"]["cases"][0][
                "raster"
            ] = MODULE._finish_fidelity(oversized)["raster"]
            malformed.append(value)

            value = copy.deepcopy(summary)
            value["reports"][1]["case_diagnostics"]["cases"][0][
                "raster"
            ]["max_channel_delta"] = 1
            malformed.append(value)

            value = copy.deepcopy(summary)
            case = value["reports"][1]["case_diagnostics"]["cases"][0]
            case["format"] = (
                "ods" if case["format"] != "ods" else "xlsx"
            )
            malformed.append(value)

            value = copy.deepcopy(summary)
            retained_by_format = value["reports"][1][
                "case_diagnostics"
            ]["retained_cases_by_format"]
            format_name = next(iter(retained_by_format))
            retained_by_format[format_name] += 1
            malformed.append(value)

            value = copy.deepcopy(summary)
            report = value["reports"][1]
            case_axis = report["case_diagnostics"]["cases"][0][
                "page_box"
            ]["by_axis"]["width"]
            total_maximum = report["page_box_geometry"]["all"][
                "by_axis"
            ]["width"]["max_delta_micropoints"]
            replacement = int(total_maximum) + 1
            case_axis.update(
                {
                    "max_delta_micropoints": replacement,
                    "min_delta_micropoints": replacement,
                    "nonzero_pages": 1,
                    "sum_delta_micropoints": replacement,
                }
            )
            malformed.append(value)

            for value in malformed:
                with self.assertRaises(MODULE.SummaryError):
                    MODULE._validate_output(value)

    def test_truncated_residuals_must_be_mathematically_realizable(
        self,
    ) -> None:
        def exact_fidelity(
            *,
            workbooks: int,
            pages: int,
            pixels: int,
        ) -> dict[str, object]:
            accumulator = MODULE._new_fidelity_accumulator()
            accumulator["workbooks"] = workbooks
            accumulator["pages"] = pages
            accumulator["raster"]["pixels"] = pixels
            accumulator["raster"]["exact_pages"] = pages
            return MODULE._finish_fidelity(accumulator)

        retained_capacity = exact_fidelity(
            workbooks=1,
            pages=1,
            pixels=1,
        )
        semantic_capacity_accumulator = (
            MODULE._new_fidelity_accumulator()
        )
        semantic_capacity_accumulator["workbooks"] = 2
        semantic_capacity_accumulator["pages"] = 2
        semantic_capacity_accumulator["raster"]["pixels"] = 2
        semantic_capacity_accumulator["raster"]["exact_pages"] = 2
        semantic_overflow = (
            MODULE.MAX_SEMANTIC_CODEPOINTS_PER_WORKBOOK + 1
        )
        for key in (
            "rxls_items",
            "libreoffice_items",
            "matched_items",
        ):
            semantic_capacity_accumulator[
                "semantic_visible_characters"
            ][key] = semantic_overflow
        semantic_capacity = MODULE._finish_fidelity(
            semantic_capacity_accumulator
        )
        MODULE._validate_fidelity_cohort(
            semantic_capacity,
            workbook_limit=2,
            code="test_residual",
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_fidelity_subset(
                retained_capacity,
                semantic_capacity,
                "test_residual",
            )

        text_capacity_accumulator = (
            MODULE._new_fidelity_accumulator()
        )
        text_capacity_accumulator["workbooks"] = 2
        text_capacity_accumulator["pages"] = 2
        text_capacity_accumulator["raster"]["pixels"] = 2
        text_capacity_accumulator["raster"]["exact_pages"] = 2
        text_overflow = MODULE.MAX_POPPLER_ITEMS_PER_PAGE + 1
        for key in (
            "rxls_items",
            "libreoffice_items",
            "matched_items",
        ):
            text_capacity_accumulator["poppler_words"][
                key
            ] = text_overflow
        text_capacity = MODULE._finish_fidelity(
            text_capacity_accumulator
        )
        MODULE._validate_fidelity_cohort(
            text_capacity,
            workbook_limit=2,
            code="test_residual",
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_fidelity_subset(
                retained_capacity,
                text_capacity,
                "test_residual",
            )

        raster_capacity = exact_fidelity(
            workbooks=2,
            pages=2,
            pixels=MODULE.MAX_RASTER_PIXELS_PER_PAGE + 2,
        )
        MODULE._validate_fidelity_cohort(
            raster_capacity,
            workbook_limit=2,
            code="test_residual",
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_fidelity_subset(
                retained_capacity,
                raster_capacity,
                "test_residual",
            )

        retained_accumulator = MODULE._new_fidelity_accumulator()
        retained_accumulator["workbooks"] = 1
        retained_accumulator["pages"] = 1
        retained_accumulator["raster"].update(
            {
                "absolute_error_sum": 10,
                "changed_pixels": 1,
                "exact_pages": 0,
                "max_channel_delta": 10,
                "pixels": 100,
            }
        )
        retained_fidelity = MODULE._finish_fidelity(
            retained_accumulator
        )
        total_accumulator = MODULE._new_fidelity_accumulator()
        total_accumulator["workbooks"] = 2
        total_accumulator["pages"] = 2
        total_accumulator["raster"].update(
            {
                "absolute_error_sum": 11,
                "changed_pixels": 1,
                "exact_pages": 1,
                "max_channel_delta": 10,
                "pixels": 200,
            }
        )
        total_fidelity = MODULE._finish_fidelity(
            total_accumulator
        )
        MODULE._validate_fidelity_cohort(
            retained_fidelity,
            workbook_limit=1,
            code="test_residual",
        )
        MODULE._validate_fidelity_cohort(
            total_fidelity,
            workbook_limit=2,
            code="test_residual",
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_fidelity_subset(
                retained_fidelity,
                total_fidelity,
                "test_residual",
            )

        retained_page_capacity = exact_fidelity(
            workbooks=64,
            pages=64,
            pixels=64,
        )
        total_page_capacity = exact_fidelity(
            workbooks=65,
            pages=65 * MODULE.MAX_PAGE_COUNT,
            pixels=65 * MODULE.MAX_PAGE_COUNT,
        )
        MODULE._validate_fidelity_cohort(
            retained_page_capacity,
            workbook_limit=64,
            code="test_residual",
        )
        MODULE._validate_fidelity_cohort(
            total_page_capacity,
            workbook_limit=65,
            code="test_residual",
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_fidelity_subset(
                retained_page_capacity,
                total_page_capacity,
                "test_residual",
            )

        def page_box(
            *,
            workbooks: int,
            width_values: list[int],
        ) -> dict[str, object]:
            def axis(values: list[int]) -> dict[str, object]:
                histogram = Counter(
                    MODULE._page_box_geometry_bucket(value)
                    for value in values
                )
                return {
                    "histogram": [
                        histogram[bucket]
                        for bucket in MODULE.PAGE_BOX_GEOMETRY_BUCKET_ORDER
                    ],
                    "max_delta_micropoints": max(values),
                    "min_delta_micropoints": min(values),
                    "nonzero_pages": sum(value != 0 for value in values),
                    "sum_delta_micropoints": sum(values),
                }

            return {
                "by_axis": {
                    "height": axis([0] * len(width_values)),
                    "width": axis(width_values),
                },
                "pages": len(width_values),
                "workbooks": workbooks,
            }

        retained_page_box = page_box(
            workbooks=1,
            width_values=[-1, -1],
        )
        total_page_box = page_box(
            workbooks=2,
            width_values=[-1, 1, 1, 1, 0, 0],
        )
        retained_page_box = MODULE._validate_page_box_geometry_cohort(
            retained_page_box,
            workbook_limit=1,
            include_histogram=True,
        )
        total_page_box = MODULE._validate_page_box_geometry_cohort(
            total_page_box,
            workbook_limit=2,
            include_histogram=True,
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_page_box_subset(
                retained_page_box,
                total_page_box,
                "test_residual",
            )

        retained_absent_bucket = (
            MODULE._validate_page_box_geometry_cohort(
                page_box(
                    workbooks=1,
                    width_values=[-3_000_000],
                ),
                workbook_limit=1,
                include_histogram=True,
            )
        )
        total_without_bucket = (
            MODULE._validate_page_box_geometry_cohort(
                page_box(
                    workbooks=2,
                    width_values=[
                        -6_000_000,
                        -6_000_000,
                        -500_000,
                        -500_000,
                    ],
                ),
                workbook_limit=2,
                include_histogram=True,
            )
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_page_box_subset(
                retained_absent_bucket,
                total_without_bucket,
                "test_residual",
            )

        retained_page_box_capacity = (
            MODULE._validate_page_box_geometry_cohort(
                page_box(
                    workbooks=64,
                    width_values=[0] * 64,
                ),
                workbook_limit=64,
                include_histogram=True,
            )
        )
        total_page_box_capacity = (
            MODULE._validate_page_box_geometry_cohort(
                page_box(
                    workbooks=65,
                    width_values=[
                        0
                    ]
                    * (65 * MODULE.MAX_PAGE_COUNT),
                ),
                workbook_limit=65,
                include_histogram=True,
            )
        )
        with self.assertRaisesRegex(
            MODULE.SummaryError, "test_residual"
        ):
            MODULE._require_page_box_subset(
                retained_page_box_capacity,
                total_page_box_capacity,
                "test_residual",
            )

    def test_geometry_summary_is_aggregate_only_and_boundary_exact(self) -> None:
        rows = _pilot_rows()
        for index, row in enumerate(rows):
            if index not in {1, 2, 3}:
                _as_premeasurement_error(row)
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
        self.assertEqual(geometry["mismatch_pages"], 1)
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

    def test_cross_document_xhtml_noise_is_separate_from_box_mismatches(
        self,
    ) -> None:
        rows = _pilot_rows()
        for index, row in enumerate(rows):
            if index not in {1, 2}:
                _as_premeasurement_error(row)
        _with_geometry(
            rows[1],
            [
                _geometry_page(
                    xhtml_cross_document_width_delta=Fraction(
                        365, 1_000_000
                    )
                )
            ],
        )
        _with_geometry(
            rows[2],
            [
                _geometry_page(
                    xhtml_cross_document_width_delta=Fraction(
                        1001, 1_000_000
                    )
                )
            ],
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
        self.assertEqual(geometry["mismatch_pages"], 0)
        self.assertEqual(
            geometry["max_direct_absolute_delta_micropoints"], 0
        )
        self.assertEqual(
            geometry["max_internal_xhtml_crosscheck_micropoints"],
            1_001,
        )
        self.assertEqual(
            geometry["by_delta"]["xhtml_width"],
            {
                "max_absolute_micropoints": 1_001,
                "nonzero_pages": 2,
            },
        )

    def test_page_box_geometry_is_signed_and_grouped_by_reviewed_cohorts(
        self,
    ) -> None:
        rows = _pilot_rows()
        for index, row in enumerate(rows):
            if index not in {1, 2, 3}:
                _as_premeasurement_error(row)
        rows[1]["features"] = sorted(
            (
                "column-width",
                "latin-text",
                "number-cell",
                "row-height",
            )
        )
        rows[2]["features"] = sorted(
            (
                "hidden-column",
                "hidden-row",
                "latin-text",
                "number-cell",
            )
        )
        rows[3]["features"] = sorted(
            (
                "chart",
                "image-drawing",
                "latin-text",
                "number-cell",
            )
        )
        _with_geometry(
            rows[1],
            [
                _geometry_page(
                    crop_height_delta=Fraction(-1, 4),
                    crop_width_delta=Fraction(1, 2),
                )
            ],
        )
        _with_geometry(
            rows[2],
            [
                _geometry_page(
                    crop_height_delta=Fraction(1, 8),
                    crop_width_delta=Fraction(-1, 4),
                )
            ],
        )
        _with_geometry(
            rows[3],
            [
                _geometry_page(crop_width_delta=Fraction(1, 10)),
                _geometry_page(crop_width_delta=Fraction(-1, 20)),
            ],
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

        page_box = summary["reports"][1]["page_box_geometry"]
        self.assertEqual(
            {
                key: page_box[key]
                for key in MODULE.PAGE_BOX_GEOMETRY_POLICY
            },
            MODULE.PAGE_BOX_GEOMETRY_POLICY,
        )
        self.assertEqual(
            set(page_box["by_format"]), {"xls", "xlsb", "xlsx"}
        )
        self.assertEqual(
            set(page_box["by_feature"]),
            {
                "chart",
                "column-width",
                "hidden-column",
                "hidden-row",
                "image-drawing",
                "row-height",
            },
        )
        def aggregate_axis(axis: dict[str, object]) -> dict[str, object]:
            return {
                key: value
                for key, value in axis.items()
                if key != "histogram"
            }

        def histogram(axis: dict[str, object]) -> dict[str, int]:
            counts = axis["histogram"]
            self.assertEqual(
                len(counts),
                MODULE.MAX_PAGE_BOX_GEOMETRY_HISTOGRAM_BUCKETS,
            )
            return dict(
                zip(
                    MODULE.PAGE_BOX_GEOMETRY_BUCKET_ORDER,
                    counts,
                    strict=True,
                )
            )

        self.assertEqual(page_box["all"]["pages"], 4)
        self.assertEqual(page_box["all"]["workbooks"], 3)
        self.assertEqual(
            aggregate_axis(page_box["all"]["by_axis"]["height"]),
            {
                "max_delta_micropoints": 125_000,
                "min_delta_micropoints": -250_000,
                "nonzero_pages": 2,
                "sum_delta_micropoints": -125_000,
            },
        )
        height_histogram = histogram(
            page_box["all"]["by_axis"]["height"]
        )
        self.assertEqual(
            {
                bucket: count
                for bucket, count in height_histogram.items()
                if count
            },
            {
                "negative_0_1_to_1_points": 1,
                "positive_0_1_to_1_points": 1,
                "zero": 2,
            },
        )
        self.assertEqual(
            aggregate_axis(page_box["all"]["by_axis"]["width"]),
            {
                "max_delta_micropoints": 500_000,
                "min_delta_micropoints": -250_000,
                "nonzero_pages": 4,
                "sum_delta_micropoints": 300_000,
            },
        )
        width_histogram = histogram(
            page_box["all"]["by_axis"]["width"]
        )
        self.assertEqual(
            {
                bucket: count
                for bucket, count in width_histogram.items()
                if count
            },
            {
                "negative_0_1_to_1_points": 1,
                "negative_up_to_0_1_points": 1,
                "positive_0_1_to_1_points": 1,
                "positive_up_to_0_1_points": 1,
            },
        )
        self.assertEqual(
            aggregate_axis(
                page_box["by_format"]["xls"]["by_axis"]["width"]
            ),
            {
                "max_delta_micropoints": 500_000,
                "min_delta_micropoints": 500_000,
                "nonzero_pages": 1,
                "sum_delta_micropoints": 500_000,
            },
        )
        self.assertEqual(
            {
                bucket: count
                for bucket, count in histogram(
                    page_box["by_format"]["xlsx"]["by_axis"]["width"]
                ).items()
                if count
            },
            {
                "negative_up_to_0_1_points": 1,
                "positive_up_to_0_1_points": 1,
            },
        )

        def aggregate_cohort(
            cohort: dict[str, object],
        ) -> dict[str, object]:
            return {
                "by_axis": {
                    axis: aggregate_axis(axis_value)
                    for axis, axis_value in cohort["by_axis"].items()
                },
                "pages": cohort["pages"],
                "workbooks": cohort["workbooks"],
            }

        for feature in ("chart", "image-drawing"):
            self.assertEqual(
                page_box["by_feature"][feature],
                aggregate_cohort(page_box["by_format"]["xlsx"]),
            )
        self.assertEqual(
            page_box["by_feature"]["column-width"],
            aggregate_cohort(page_box["by_format"]["xls"]),
        )
        self.assertEqual(
            page_box["by_feature"]["hidden-column"],
            aggregate_cohort(page_box["by_format"]["xlsb"]),
        )
        self.assertEqual(
            page_box["by_feature"]["hidden-row"],
            aggregate_cohort(page_box["by_format"]["xlsb"]),
        )
        self.assertEqual(
            page_box["by_feature"]["row-height"],
            aggregate_cohort(page_box["by_format"]["xls"]),
        )
        for cohort in page_box["by_feature"].values():
            for axis in cohort["by_axis"].values():
                self.assertNotIn("histogram", axis)
        rendered = MODULE._json(summary).decode("ascii")
        for forbidden in (
            "600/1",
            "450/1",
            "/srv/private",
            "customer-",
            "pdf_point_geometry",
        ):
            self.assertNotIn(forbidden, rendered)

    def test_page_box_histogram_boundaries_and_rounding_are_exact(
        self,
    ) -> None:
        suffixes = (
            "up_to_0_1_points",
            "0_1_to_1_points",
            "1_to_5_points",
            "5_to_10_points",
            "10_to_25_points",
            "25_to_50_points",
            "50_to_100_points",
        )
        self.assertEqual(MODULE._page_box_geometry_bucket(0), "zero")
        for sign, prefix in ((-1, "negative"), (1, "positive")):
            self.assertEqual(
                MODULE._page_box_geometry_bucket(sign),
                f"{prefix}_up_to_0_1_points",
            )
            for index, upper in enumerate(
                MODULE.PAGE_BOX_GEOMETRY_MAGNITUDE_UPPER_BOUNDS_MICROPOINTS
            ):
                with self.subTest(sign=sign, upper=upper):
                    self.assertEqual(
                        MODULE._page_box_geometry_bucket(
                            sign * upper
                        ),
                        f"{prefix}_{suffixes[index]}",
                    )
                    next_suffix = (
                        suffixes[index + 1]
                        if index + 1 < len(suffixes)
                        else "over_100_points"
                    )
                    self.assertEqual(
                        MODULE._page_box_geometry_bucket(
                            sign * (upper + 1)
                        ),
                        f"{prefix}_{next_suffix}",
                    )
            self.assertEqual(
                MODULE._page_box_geometry_bucket(
                    sign * MODULE.MAX_POINT_DELTA_MICROPOINTS
                ),
                f"{prefix}_over_100_points",
            )
        self.assertEqual(
            MODULE._signed_ceil_micropoints(
                Fraction(1, 2_000_000)
            ),
            1,
        )
        self.assertEqual(
            MODULE._signed_ceil_micropoints(
                Fraction(-1, 2_000_000)
            ),
            -1,
        )
        for value in (
            -MODULE.MAX_POINT_DELTA_MICROPOINTS - 1,
            MODULE.MAX_POINT_DELTA_MICROPOINTS + 1,
        ):
            with self.assertRaisesRegex(
                MODULE.SummaryError,
                r"\Apage_box_geometry_delta_limit\Z",
            ):
                MODULE._page_box_geometry_bucket(value)

    def test_page_box_histogram_has_fixed_cardinality_and_order(
        self,
    ) -> None:
        def summary(reverse: bool) -> dict[str, object]:
            rows = _pilot_rows()
            for index, row in enumerate(rows):
                if index != 1:
                    _as_premeasurement_error(row)
            pages = [
                _geometry_page(
                    crop_width_delta=Fraction(index, 1_000_000)
                )
                for index in range(1, 41)
            ]
            if reverse:
                pages.reverse()
            _with_geometry(rows[1], pages)
            return _summarize_pilot(rows)

        forward = summary(False)
        reverse = summary(True)
        self.assertEqual(forward, reverse)
        page_box = forward["reports"][1]["page_box_geometry"]
        axis = page_box["all"]["by_axis"]["width"]
        self.assertEqual(
            len(axis["histogram"]),
            MODULE.MAX_PAGE_BOX_GEOMETRY_HISTOGRAM_BUCKETS,
        )
        counts = dict(
            zip(
                MODULE.PAGE_BOX_GEOMETRY_BUCKET_ORDER,
                axis["histogram"],
                strict=True,
            )
        )
        self.assertEqual(
            {
                bucket: count
                for bucket, count in counts.items()
                if count
            },
            {"positive_up_to_0_1_points": 40},
        )
        rendered = MODULE._json(forward).decode("ascii")
        self.assertNotIn("/srv/private", rendered)
        self.assertNotIn("customer-1", rendered)
        self.assertNotIn(
            hashlib.sha256(b"case-1").hexdigest(),
            rendered,
        )
        self.assertNotIn("1/1000000", rendered)

    def test_page_box_histogram_output_is_fail_closed(self) -> None:
        rows = _pilot_rows()
        for index, row in enumerate(rows):
            if index != 1:
                _as_premeasurement_error(row)
        rows[1]["features"] = sorted(
            ("column-width", "latin-text", "number-cell")
        )
        _with_geometry(
            rows[1],
            [
                _geometry_page(
                    crop_width_delta=Fraction(-1, 20)
                ),
                _geometry_page(crop_width_delta=Fraction(1, 5)),
                _geometry_page(crop_width_delta=Fraction(1, 2)),
            ],
        )
        summary = _summarize_pilot(rows)
        bucket_index = {
            bucket: index
            for index, bucket in enumerate(
                MODULE.PAGE_BOX_GEOMETRY_BUCKET_ORDER
            )
        }

        def all_width(document: dict[str, object]) -> dict[str, object]:
            return document["reports"][1]["page_box_geometry"][
                "all"
            ]["by_axis"]["width"]

        def format_width(
            document: dict[str, object],
        ) -> dict[str, object]:
            return document["reports"][1]["page_box_geometry"][
                "by_format"
            ]["xls"]["by_axis"]["width"]

        malformed: list[tuple[str, dict[str, object]]] = []

        value = copy.deepcopy(summary)
        value["reports"][1]["page_box_geometry"]["histogram"][
            "bucket_order"
        ][0] = "private_case_bucket"
        malformed.append(("policy-bucket", value))

        value = copy.deepcopy(summary)
        value["reports"][1]["page_box_geometry"]["histogram"][
            "max_buckets_per_axis"
        ] = True
        malformed.append(("policy-bool", value))

        value = copy.deepcopy(summary)
        all_width(value)["histogram"][
            bucket_index["negative_up_to_0_1_points"]
        ] = True
        malformed.append(("count-bool", value))

        value = copy.deepcopy(summary)
        all_width(value)["histogram"].pop()
        malformed.append(("bucket-removed", value))

        value = copy.deepcopy(summary)
        all_width(value)["histogram"].append(0)
        malformed.append(("bucket-appended", value))

        value = copy.deepcopy(summary)
        all_width(value)["histogram"][bucket_index["zero"]] += 1
        malformed.append(("count-total", value))

        value = copy.deepcopy(summary)
        all_width(value)["nonzero_pages"] -= 1
        malformed.append(("nonzero-count", value))

        value = copy.deepcopy(summary)
        all_width(value)["min_delta_micropoints"] = -2_000_000
        malformed.append(("minimum-band", value))

        value = copy.deepcopy(summary)
        all_width(value)["sum_delta_micropoints"] = 500_000
        format_width(value)["sum_delta_micropoints"] = 500_000
        malformed.append(("infeasible-band-sum", value))

        value = copy.deepcopy(summary)
        axis = all_width(value)
        axis["histogram"][
            bucket_index["positive_0_1_to_1_points"]
        ] -= 1
        axis["histogram"][
            bucket_index["positive_up_to_0_1_points"]
        ] += 1
        axis["sum_delta_micropoints"] = 500_000
        malformed.append(("format-partition", value))

        value = copy.deepcopy(summary)
        value["reports"][1]["page_box_geometry"]["by_feature"][
            "column-width"
        ]["by_axis"]["width"]["histogram"] = list(
            all_width(value)["histogram"]
        )
        malformed.append(("feature-histogram", value))

        value = copy.deepcopy(summary)
        value["reports"][1]["page_box_geometry"]["by_format"].pop(
            "xls"
        )
        malformed.append(("format-coverage", value))

        value = copy.deepcopy(summary)
        value["schema"] = "rxls.render-oracle-failure-summary.v9"
        malformed.append(("old-schema", value))

        for label, document in malformed:
            with self.subTest(label=label):
                with self.assertRaises(MODULE.SummaryError):
                    MODULE._validate_output(document)

    def test_empty_page_box_histograms_are_fixed_and_private_policy_isolated(
        self,
    ) -> None:
        summary = MODULE.summarize(
            Path("/definitely/missing"),
            profile="full",
            baseline_mode="candidate",
            head_sha=HEAD_SHA,
        )
        expected_policy = copy.deepcopy(
            MODULE.PAGE_BOX_GEOMETRY_POLICY
        )
        page_boxes = [
            report["page_box_geometry"]
            for report in summary["reports"]
        ]
        for page_box in page_boxes:
            self.assertEqual(page_box["by_feature"], {})
            self.assertEqual(page_box["by_format"], {})
            for axis in page_box["all"]["by_axis"].values():
                self.assertEqual(
                    axis["histogram"],
                    [0]
                    * MODULE.MAX_PAGE_BOX_GEOMETRY_HISTOGRAM_BUCKETS,
                )
        self.assertIsNot(
            page_boxes[0]["histogram"],
            MODULE.PAGE_BOX_GEOMETRY_POLICY["histogram"],
        )
        self.assertIsNot(
            page_boxes[0]["histogram"],
            page_boxes[1]["histogram"],
        )
        page_boxes[0]["histogram"]["bucket_order"][0] = (
            "private_case_bucket"
        )
        self.assertEqual(
            MODULE.PAGE_BOX_GEOMETRY_POLICY,
            expected_policy,
        )
        self.assertEqual(
            page_boxes[1]["histogram"],
            expected_policy["histogram"],
        )
        with self.assertRaises(MODULE.SummaryError):
            MODULE._validate_output(summary)

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
                _case_id_key_for_test=TEST_CASE_ID_KEY,
            )
            summary_b = MODULE.summarize(
                second,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
                _case_id_key_for_test=TEST_CASE_ID_KEY,
            )

        self.assertEqual(summary_a, summary_b)
        rendered = MODULE._json(summary_b).decode("ascii")
        for forbidden in (
            "/private/tenant",
            "private workbook content",
            '"sha256":',
            '"path"',
        ):
            self.assertNotIn(forbidden, rendered)

    def test_unique_text_geometry_is_merged_for_all_and_format_only(
        self,
    ) -> None:
        rows = _pilot_rows()
        for index, row in enumerate(rows):
            if index not in {1, 3}:
                _as_premeasurement_error(row)
        first_page = _with_unique_text_geometry(
            _geometry_page(),
            word_histogram=((-2, 1), (1, 1)),
            line_histogram=((0, 2),),
        )
        second_page = _with_unique_text_geometry(
            _geometry_page(),
            word_histogram=((0, 1), (2, 1)),
            line_histogram=((-1, 1), (1, 1)),
        )
        _with_geometry(rows[1], [first_page])
        _with_geometry(rows[3], [second_page])
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

        parity = summary["reports"][1]
        self.assertEqual(
            set(parity["word_geometry"]), {"all", "by_format"}
        )
        self.assertEqual(
            set(parity["word_geometry"]["by_format"]),
            {"xls", "xlsx"},
        )
        word = parity["word_geometry"]["all"]
        self.assertEqual(
            {
                key: word[key]
                for key in (
                    "libreoffice_unique_items",
                    "matched_items",
                    "pages",
                    "rxls_unique_items",
                    "workbooks",
                )
            },
            {
                "libreoffice_unique_items": 8,
                "matched_items": 4,
                "pages": 2,
                "rxls_unique_items": 6,
                "workbooks": 2,
            },
        )
        expected_axis = {
            "exact": {
                "count": 4,
                "max_delta_millipoints": 2,
                "min_delta_millipoints": -2,
                "negative_overflow_items": 0,
                "positive_overflow_items": 0,
                "sum_delta_millipoints": 1,
            },
            "histogram": [
                {"count": 1, "delta_millipoints": -2},
                {"count": 1, "delta_millipoints": 0},
                {"count": 1, "delta_millipoints": 1},
                {"count": 1, "delta_millipoints": 2},
            ],
        }
        for axis in MODULE.TEXT_GEOMETRY_AXES:
            self.assertEqual(word["by_axis"][axis], expected_axis)
        line = parity["line_geometry"]["all"]
        self.assertEqual(line["workbooks"], 2)
        self.assertEqual(line["pages"], 2)
        self.assertEqual(line["matched_items"], 4)
        self.assertEqual(
            line["by_axis"]["center_y"]["exact"][
                "sum_delta_millipoints"
            ],
            0,
        )
        self.assertEqual(
            set(parity["by_format"]["xls"]),
            {"by_classification", "workbooks"},
        )
        self.assertEqual(
            set(parity["by_feature"]["latin-text"]),
            {"by_classification", "workbooks"},
        )
        rendered = MODULE._json(summary).decode("ascii")
        for forbidden in (
            "/srv/private",
            "private workbook content",
            "text_box_unique_geometry",
            "text_line_box_unique_geometry",
            '"by_feature": {\n          "line_geometry"',
        ):
            self.assertNotIn(forbidden, rendered)

    def test_compared_rows_require_complete_metric_geometry(self) -> None:
        def omit_row_geometry(row: dict[str, object]) -> None:
            row.pop("metrics")
            row.pop("pages")

        mutations = {
            "row": omit_row_geometry,
            "point": lambda row: row["pages"][0].pop(
                "pdf_point_geometry"
            ),
            "word": lambda row: row["pages"][0].pop(
                "text_box_unique_geometry"
            ),
            "line": lambda row: row["pages"][0].pop(
                "text_line_box_unique_geometry"
            ),
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as raw:
                hosted = Path(raw)
                rows = _pilot_rows()
                mutation(rows[1])
                _write(
                    hosted / "parity-report-a.json",
                    _report(rows, profile="pilot", label="parity-a"),
                )
                with self.assertRaises(MODULE.SummaryError) as raised:
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )
                self.assertNotIn("/srv/private", str(raised.exception))

    def test_different_rows_require_geometry_and_errors_must_omit_it(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            rows = _pilot_rows()
            rows[1]["classification"] = "below_similarity_threshold"
            rows[1]["status"] = "different"
            rows[1].pop("metrics")
            rows[1].pop("pages")
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            with self.assertRaisesRegex(
                MODULE.SummaryError, "metric_geometry_missing"
            ):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

            _as_premeasurement_error(rows[1])
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
                _case_id_key_for_test=TEST_CASE_ID_KEY,
            )

            _with_geometry(rows[1], [_geometry_page()])
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            with_incomparable_geometry = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
                _case_id_key_for_test=TEST_CASE_ID_KEY,
            )
            self.assertEqual(with_incomparable_geometry, summary)
        self.assertEqual(summary["reports"][1]["by_status"]["error"], 2)
        self.assertEqual(summary["reports"][1]["geometry"]["workbooks"], 38)

    def test_unique_text_geometry_input_is_exact_ordered_and_bounded(
        self,
    ) -> None:
        def rows() -> list[dict[str, object]]:
            value = _pilot_rows()
            page = _with_unique_text_geometry(
                _geometry_page(),
                word_histogram=((-2, 1), (1, 1)),
                line_histogram=((0, 2),),
            )
            _with_geometry(value[1], [page])
            return value

        def word(row: dict[str, object]) -> dict[str, object]:
            return row["pages"][0]["text_box_unique_geometry"]

        def impossible_bucket_sum(row: dict[str, object]) -> None:
            geometry = word(row)
            geometry["delta_histograms_millipoints"]["x_min"] = [
                {"count": 1, "delta_millipoints": -500},
                {"count": 1, "delta_millipoints": 500},
            ]
            geometry["exact_delta_summaries_millipoints"]["x_min"].update(
                {
                    "max_delta_millipoints": 749,
                    "min_delta_millipoints": -3,
                    "sum_delta_millipoints": 1_498,
                }
            )

        def impossible_axis_identity(row: dict[str, object]) -> None:
            geometry = row["pages"][0][
                "text_line_box_unique_geometry"
            ]
            geometry["delta_histograms_millipoints"]["center_x"] = [
                {"count": 2, "delta_millipoints": 1}
            ]
            geometry["exact_delta_summaries_millipoints"][
                "center_x"
            ].update(
                {
                    "max_delta_millipoints": 1,
                    "min_delta_millipoints": 1,
                    "sum_delta_millipoints": 2,
                }
            )

        mutations = {
            "object-extra": lambda row: word(row).__setitem__(
                "private_path", "/srv/private/customer.xlsx"
            ),
            "axis-missing": lambda row: word(row)[
                "delta_histograms_millipoints"
            ].pop("x_min"),
            "axis-extra": lambda row: word(row)[
                "delta_histograms_millipoints"
            ].__setitem__("private_axis", []),
            "exact-axis-missing": lambda row: word(row)[
                "exact_delta_summaries_millipoints"
            ].pop("x_min"),
            "exact-summary-extra": lambda row: word(row)[
                "exact_delta_summaries_millipoints"
            ]["x_min"].__setitem__("private_path", "/srv/private"),
            "exact-count-drift": lambda row: word(row)[
                "exact_delta_summaries_millipoints"
            ]["x_min"].__setitem__("count", 1),
            "exact-sum-drift": lambda row: word(row)[
                "exact_delta_summaries_millipoints"
            ]["x_min"].__setitem__("sum_delta_millipoints", 99),
            "exact-extrema-bucket-drift": lambda row: word(row)[
                "exact_delta_summaries_millipoints"
            ]["x_min"].update(
                {
                    "max_delta_millipoints": 999,
                    "min_delta_millipoints": 999,
                    "sum_delta_millipoints": 1_998,
                }
            ),
            "exact-impossible-bucket-sum": impossible_bucket_sum,
            "exact-impossible-axis-identity": impossible_axis_identity,
            "histogram-not-list": lambda row: word(row)[
                "delta_histograms_millipoints"
            ].__setitem__("x_min", "/srv/private/customer.xlsx"),
            "bucket-extra": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][0].__setitem__(
                "content", "private workbook content"
            ),
            "unordered": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"].reverse(),
            "duplicate-delta": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][1].__setitem__("delta_millipoints", -2),
            "boolean-delta": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][0].__setitem__("delta_millipoints", True),
            "delta-limit": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][1].__setitem__(
                "delta_millipoints",
                MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS + 1,
            ),
            "unrecognized-bucket": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][1].__setitem__("delta_millipoints", 11),
            "zero-count": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][0].__setitem__("count", 0),
            "boolean-count": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][0].__setitem__("count", True),
            "population-drift": lambda row: word(row)[
                "delta_histograms_millipoints"
            ]["x_min"][0].__setitem__("count", 2),
            "matched-over-unique": lambda row: word(row).__setitem__(
                "rxls_unique_items", 1
            ),
            "unique-over-rxls-items": lambda row: row["pages"][
                0
            ].__setitem__("text_box_rxls_items", 1),
            "unique-over-libreoffice-items": lambda row: row["pages"][
                0
            ].__setitem__("text_box_libreoffice_items", 1),
            "unique-over-paired-items": lambda row: row["pages"][
                0
            ].__setitem__("text_box_matched_items", 1),
            "missing-line-object": lambda row: row["pages"][0].pop(
                "text_line_box_unique_geometry"
            ),
            "missing-both-objects": lambda row: (
                row["pages"][0].pop("text_box_unique_geometry"),
                row["pages"][0].pop("text_line_box_unique_geometry"),
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
                with self.assertRaises(MODULE.SummaryError) as raised:
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )
                message = str(raised.exception)
                self.assertNotIn("/srv/private", message)
                self.assertNotIn("private workbook content", message)
                self.assertNotIn("customer.xlsx", message)

        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            value = rows()
            _write(
                hosted / "parity-report-a.json",
                _report(value, profile="pilot", label="parity-a"),
            )
            with mock.patch.object(
                MODULE, "MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS", 1
            ):
                with self.assertRaisesRegex(
                    MODULE.SummaryError, "text_geometry_page"
                ):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            value = _pilot_rows()
            _with_geometry(
                value[1],
                [
                    _with_unique_text_geometry(
                        _geometry_page(),
                        word_histogram=((-500, 1), (500, 1)),
                        line_histogram=((-500, 1), (500, 1)),
                    )
                ],
            )
            _with_geometry(
                value[3],
                [
                    _with_unique_text_geometry(
                        _geometry_page(),
                        word_histogram=((-1_000, 1), (1_000, 1)),
                        line_histogram=((-1_000, 1), (1_000, 1)),
                    )
                ],
            )
            _write(
                hosted / "parity-report-a.json",
                _report(value, profile="pilot", label="parity-a"),
            )
            with mock.patch.object(
                MODULE, "MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS", 2
            ):
                with self.assertRaisesRegex(
                    MODULE.SummaryError, "text_geometry_bucket_limit"
                ):
                    MODULE.summarize(
                        hosted,
                        profile="pilot",
                        baseline_mode="verify",
                        head_sha=HEAD_SHA,
                    )

    def test_unique_text_geometry_preserves_exact_overflow_statistics(
        self,
    ) -> None:
        rows = _pilot_rows()
        page = _with_unique_text_geometry(
            _geometry_page(),
            word_histogram=((-12_000, 1), (12_000, 1)),
            line_histogram=((-12_000, 1), (12_000, 1)),
        )
        for key in (
            "text_box_unique_geometry",
            "text_line_box_unique_geometry",
        ):
            for exact in page[key][
                "exact_delta_summaries_millipoints"
            ].values():
                exact.update(
                    {
                        "max_delta_millipoints": (
                            MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
                        ),
                        "min_delta_millipoints": (
                            -MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
                        ),
                        "sum_delta_millipoints": 0,
                    }
                )
        _with_geometry(rows[1], [page])
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            document = _report(
                rows, profile="pilot", label="parity-a"
            )
            _write(hosted / "parity-report-a.json", document)
            summary = MODULE.summarize(
                hosted,
                profile="pilot",
                baseline_mode="verify",
                head_sha=HEAD_SHA,
            )

        axis = summary["reports"][1]["word_geometry"]["all"][
            "by_axis"
        ]["x_min"]
        self.assertEqual(
            axis["histogram"],
            [
                {"count": 1, "delta_millipoints": -12_000},
                {"count": 1, "delta_millipoints": 12_000},
            ],
        )
        self.assertEqual(
            axis["exact"],
            {
                "count": 2,
                "max_delta_millipoints": (
                    MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
                ),
                "min_delta_millipoints": (
                    -MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
                ),
                "negative_overflow_items": 1,
                "positive_overflow_items": 1,
                "sum_delta_millipoints": 0,
            },
        )

        document["files"][1]["pages"][0][
            "text_box_unique_geometry"
        ]["exact_delta_summaries_millipoints"]["x_min"][
            "negative_overflow_items"
        ] = 0
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(hosted / "parity-report-a.json", document)
            with self.assertRaisesRegex(
                MODULE.SummaryError, "text_geometry_page"
            ):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

    def test_unique_text_geometry_output_is_fully_recomputed(self) -> None:
        rows = _pilot_rows()
        page = _with_unique_text_geometry(
            _geometry_page(),
            word_histogram=((-2, 1), (1, 1)),
            line_histogram=((0, 2),),
        )
        _with_geometry(rows[1], [page])
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

        injected = copy.deepcopy(summary)
        injected["reports"][1]["word_geometry"]["by_feature"] = {}
        reordered = copy.deepcopy(summary)
        reordered["reports"][1]["word_geometry"]["all"]["by_axis"][
            "x_min"
        ]["histogram"].reverse()
        mean_drift = copy.deepcopy(summary)
        mean_drift["reports"][1]["word_geometry"]["all"]["by_axis"][
            "x_min"
        ]["exact"]["sum_delta_millipoints"] += 1
        format_drift = copy.deepcopy(summary)
        format_drift["reports"][1]["word_geometry"]["by_format"]["xls"][
            "matched_items"
        ] -= 1
        line_coverage_drift = copy.deepcopy(summary)
        line_coverage_drift["reports"][1]["line_geometry"]["all"][
            "pages"
        ] = 0
        policy_drift = copy.deepcopy(summary)
        policy_drift["geometry_policy"]["delta_direction"] = (
            "libreoffice_minus_rxls"
        )
        for document in (
            injected,
            reordered,
            mean_drift,
            format_drift,
            line_coverage_drift,
            policy_drift,
        ):
            with self.subTest(document=document):
                with self.assertRaises(MODULE.SummaryError):
                    MODULE._validate_output(document)

    def test_full_profile_saturated_geometry_and_groups_fit_output_budget(
        self,
    ) -> None:
        buckets = sorted(MODULE.TEXT_GEOMETRY_ALLOWED_BUCKETS)
        self.assertEqual(
            len(buckets),
            MODULE.MAX_TEXT_GEOMETRY_HISTOGRAM_BUCKETS,
        )
        exact = {
            "count": len(buckets),
            "max_delta_millipoints": (
                MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
            ),
            "min_delta_millipoints": (
                -MODULE.MAX_TEXT_GEOMETRY_DELTA_MILLIPOINTS
            ),
            "negative_overflow_items": 1,
            "positive_overflow_items": 1,
            "sum_delta_millipoints": 0,
        }
        page = {
            "exact_summaries": {
                axis: copy.deepcopy(exact)
                for axis in MODULE.TEXT_GEOMETRY_AXES
            },
            "histograms": {
                axis: Counter({bucket: 1 for bucket in buckets})
                for axis in MODULE.TEXT_GEOMETRY_AXES
            },
            "libreoffice_unique_items": len(buckets),
            "matched_items": len(buckets),
            "rxls_unique_items": len(buckets),
        }

        def geometry(
            format_counts: dict[str, int],
        ) -> dict[str, object]:
            all_accumulator = MODULE._new_text_geometry_accumulator()
            by_format: dict[str, object] = {}
            for format_name, count in format_counts.items():
                accumulator = MODULE._new_text_geometry_accumulator()
                accumulator["workbooks"] = count
                all_accumulator["workbooks"] += count
                for _ in range(count):
                    MODULE._merge_text_geometry_page(
                        accumulator, page
                    )
                    MODULE._merge_text_geometry_page(
                        all_accumulator, page
                    )
                by_format[format_name] = (
                    MODULE._finish_text_geometry_cohort(accumulator)
                )
            return {
                "all": MODULE._finish_text_geometry_cohort(
                    all_accumulator
                ),
                "by_format": by_format,
            }

        zero_page = {
            "crop_box_height": Fraction(),
            "crop_box_width": Fraction(),
        }

        def page_box_geometry(
            format_counts: dict[str, int],
        ) -> dict[str, object]:
            all_accumulator = (
                MODULE._new_page_box_geometry_accumulator()
            )
            by_format: dict[str, object] = {}
            for format_name, count in format_counts.items():
                accumulator = (
                    MODULE._new_page_box_geometry_accumulator()
                )
                for _ in range(count):
                    MODULE._merge_page_box_geometry_workbook(
                        accumulator, [zero_page]
                    )
                    MODULE._merge_page_box_geometry_workbook(
                        all_accumulator, [zero_page]
                    )
                by_format[format_name] = (
                    MODULE._finish_page_box_geometry_cohort(
                        accumulator,
                        include_histogram=True,
                    )
                )
            total = sum(format_counts.values())
            by_feature: dict[str, object] = {}
            for feature in MODULE.PAGE_BOX_GEOMETRY_FEATURES:
                accumulator = (
                    MODULE._new_page_box_geometry_accumulator()
                )
                for _ in range(total):
                    MODULE._merge_page_box_geometry_workbook(
                        accumulator, [zero_page]
                    )
                by_feature[feature] = (
                    MODULE._finish_page_box_geometry_cohort(
                        accumulator,
                        include_histogram=False,
                    )
                )
            return {
                **copy.deepcopy(MODULE.PAGE_BOX_GEOMETRY_POLICY),
                "all": MODULE._finish_page_box_geometry_cohort(
                    all_accumulator,
                    include_histogram=True,
                ),
                "by_feature": by_feature,
                "by_format": by_format,
            }

        def point_geometry(total: int) -> dict[str, object]:
            value = MODULE._empty_geometry()
            value["pages"] = total
            value["workbooks"] = total
            return value

        def fidelity_cohort(count: int) -> dict[str, object]:
            accumulator = MODULE._new_fidelity_accumulator()
            accumulator["workbooks"] = count
            accumulator["pages"] = count
            accumulator["raster"]["pixels"] = count * 100
            accumulator["raster"]["exact_pages"] = count
            return MODULE._finish_fidelity(accumulator)

        def fidelity(
            format_counts: dict[str, int],
        ) -> dict[str, object]:
            by_format = {
                format_name: fidelity_cohort(count)
                for format_name, count in format_counts.items()
            }
            accumulator = MODULE._new_fidelity_accumulator()
            for cohort in by_format.values():
                MODULE._merge_fidelity(accumulator, cohort)
            return {
                "all": MODULE._finish_fidelity(accumulator),
                "by_format": by_format,
            }

        def case_diagnostics(
            label: str,
            format_counts: dict[str, int],
        ) -> dict[str, object]:
            total = sum(format_counts.values())
            format_name = next(iter(format_counts))
            single = fidelity_cohort(1)
            page_box = MODULE._new_page_box_geometry_accumulator()
            MODULE._merge_page_box_geometry_workbook(
                page_box, [zero_page]
            )
            cases = []
            for index in range(
                min(total, MODULE.MAX_CASE_DIAGNOSTICS_PER_REPORT)
            ):
                cases.append(
                    {
                        "case_id": hashlib.sha256(
                            f"{label}-{index}".encode()
                        ).hexdigest(),
                        "format": format_name,
                        "page_box": (
                            MODULE._finish_page_box_geometry_cohort(
                                page_box,
                                include_histogram=True,
                            )
                        ),
                        "poppler_lines": single["poppler_lines"],
                        "poppler_words": single["poppler_words"],
                        "raster": copy.deepcopy(single["raster"]),
                        "semantic_visible_characters": single[
                            "semantic_visible_characters"
                        ],
                    }
                )
            cases.sort(key=lambda case: case["case_id"])
            return {
                "available_cases": total,
                "available_cases_by_format": copy.deepcopy(
                    format_counts
                ),
                "cases": cases,
                "retained_cases": len(cases),
                "retained_cases_by_format": {
                    format_name: len(cases)
                },
                "truncated": len(cases) != total,
            }

        classifications = sorted(MODULE.OUTPUT_CLASSIFICATIONS)
        self.assertIn("page_count_mismatch", classifications)
        ordinary_classifications = [
            classification
            for classification in classifications
            if classification != "page_count_mismatch"
        ]

        def report(
            label: str, formats: tuple[str, ...]
        ) -> dict[str, object]:
            total = MODULE.LANES["full"][label]
            mismatch_count = total - len(ordinary_classifications)
            self.assertGreater(mismatch_count, 0)
            classification_counts = {
                classification: 1
                for classification in ordinary_classifications
            }
            classification_counts["page_count_mismatch"] = mismatch_count
            format_classes = {
                format_name: Counter()
                for format_name in formats
            }
            for index, classification in enumerate(
                ordinary_classifications
            ):
                format_classes[formats[index % len(formats)]][
                    classification
                ] = 1
            mismatch_base, mismatch_remainder = divmod(
                mismatch_count, len(formats)
            )
            for index, format_name in enumerate(formats):
                count = mismatch_base + int(index < mismatch_remainder)
                if count:
                    format_classes[format_name][
                        "page_count_mismatch"
                    ] = count
            format_counts = {
                format_name: sum(counts.values())
                for format_name, counts in format_classes.items()
            }
            mismatch_pairs = [
                (rxls_pages, libreoffice_pages)
                for rxls_pages in range(1, MODULE.MAX_PAGE_COUNT + 1)
                for libreoffice_pages in range(
                    1, MODULE.MAX_PAGE_COUNT + 1
                )
                if rxls_pages != libreoffice_pages
            ][:mismatch_count]
            self.assertEqual(len(mismatch_pairs), mismatch_count)
            value = MODULE._empty(label)
            value.update(
                {
                    "by_classification": classification_counts,
                    "by_feature": {
                        feature: {
                            "by_classification": (
                                classification_counts
                            ),
                            "workbooks": total,
                        }
                        for feature in MODULE.FEATURES
                    },
                    "by_format": {
                        format_name: {
                            "by_classification": dict(counts),
                            "workbooks": sum(counts.values()),
                        }
                        for format_name, counts in format_classes.items()
                    },
                    "by_status": {
                        "compared": len(ordinary_classifications),
                        "error": mismatch_count,
                    },
                    "case_diagnostics": case_diagnostics(
                        label,
                        format_counts,
                    ),
                    "fidelity": fidelity(format_counts),
                    "geometry": point_geometry(total),
                    "line_geometry": geometry(format_counts),
                    "page_box_geometry": page_box_geometry(
                        format_counts
                    ),
                    "page_count_mismatches": [
                        {
                            "libreoffice_pages": libreoffice_pages,
                            "rxls_pages": rxls_pages,
                            "workbooks": 1,
                        }
                        for rxls_pages, libreoffice_pages in mismatch_pairs
                    ],
                    "word_geometry": geometry(format_counts),
                    "workbooks": total,
                }
            )
            return value

        document = {
            "baseline_mode": "candidate",
            "case_id_policy": MODULE.CASE_ID_POLICY,
            "geometry_policy": MODULE.TEXT_GEOMETRY_POLICY,
            "head_sha": HEAD_SHA,
            "ingestion": {
                "expected_workbooks": 1700,
                "received_workbooks": 1700,
                "status": "complete",
            },
            "profile": "full",
            "reports": [
                report("authored-print", ("xlsx",)),
                report("parity-a", tuple(sorted(MODULE.FORMATS))),
                report("parity-b", tuple(sorted(MODULE.FORMATS))),
            ],
            "schema": MODULE.OUTPUT_SCHEMA,
        }
        MODULE._validate_output(document)
        payload = MODULE._json(document)
        self.assertLessEqual(
            len(payload),
            MODULE.MAX_OUTPUT_BYTES - 48 * 1024,
        )
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / MODULE.OUTPUT_NAME
            MODULE.write_atomic(output, document)
            self.assertEqual(output.read_bytes(), payload)

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
                "case_id_policy": MODULE.CASE_ID_POLICY,
                "geometry_policy": MODULE.TEXT_GEOMETRY_POLICY,
                "head_sha": HEAD_SHA,
                "ingestion": {
                    "expected_workbooks": 1700,
                    "received_workbooks": 0,
                    "status": "unavailable",
                },
                "profile": "full",
                "reports": [
                    MODULE._empty("authored-print"),
                    MODULE._empty("parity-a"),
                    MODULE._empty("parity-b"),
                ],
                "schema": MODULE.OUTPUT_SCHEMA,
            },
        )

    def test_returned_geometry_policy_cannot_mutate_validator_policy(
        self,
    ) -> None:
        expected_policy = copy.deepcopy(MODULE.TEXT_GEOMETRY_POLICY)
        with tempfile.TemporaryDirectory() as raw:
            summary = MODULE.summarize(
                Path(raw) / "missing",
                profile="full",
                baseline_mode="candidate",
                head_sha=HEAD_SHA,
            )

        self.assertIsNot(
            summary["geometry_policy"], MODULE.TEXT_GEOMETRY_POLICY
        )
        self.assertIsNot(
            summary["geometry_policy"]["histogram"],
            MODULE.TEXT_GEOMETRY_POLICY["histogram"],
        )
        summary["geometry_policy"]["histogram"]["rounding"] = (
            "nearest_width_multiple"
        )
        self.assertEqual(MODULE.TEXT_GEOMETRY_POLICY, expected_policy)
        with self.assertRaises(MODULE.SummaryError):
            MODULE._validate_output(summary)

    def test_ooxml_row_diagnostic_summary_is_fixed_and_content_neutral(self) -> None:
        rows = []
        automatic_features = (
            "auto-bold-font",
            "auto-bold-font-wrapped",
            "auto-heading-western-asian",
            "auto-heading-western-complex",
            "auto-large-font",
            "auto-long-unwrapped",
            "auto-numeric-color-conditional",
            "auto-numeric-no-conditional",
            "auto-wrapped-color-conditional",
            "auto-wrapped-explicit",
            "auto-wrapped-hidden",
            "auto-wrapped-image",
            "auto-wrapped-long",
            "auto-wrapped-long-anchor",
            "auto-wrapped-merged",
            "auto-wrapped-no-conditional",
            "auto-wrapped-rtl",
            "auto-wrapped-wide",
            "hidden-heading-western-asian",
            "hidden-heading-western-complex",
            "manual-heading-western-asian",
            "manual-heading-western-complex",
        )
        for index in range(34):
            features = {
                "normal-font-noto" if index < 30 else "normal-font-carlito",
                "normal-size-11" if index < 30 else "normal-size-12",
                "ooxml-implicit-row",
                "sheet-format-missing" if index < 30 else "sheet-format-present",
            }
            if index == 4:
                features.add("explicit-row-height")
            if index >= 12:
                features.add(automatic_features[index - 12])
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
        self.assertEqual(summary["reports"][1]["workbooks"], 34)
        self.assertEqual(
            summary["reports"][1]["by_classification"],
            {"measurement_geometry_stage": 1, "within_threshold": 33},
        )
        self.assertTrue(
            MODULE.DIAGNOSTIC_FEATURES.issubset(
                summary["reports"][1]["by_feature"]
            )
        )
        self.assertTrue(
            MODULE.DIAGNOSTIC_FEATURES.issubset(
                summary["reports"][1]["page_box_geometry"]["by_feature"]
            )
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

    def test_diagnostic_features_are_rejected_outside_diagnostic_profile(
        self,
    ) -> None:
        for profile in ("pilot", "full"):
            rows = [
                _row(
                    index,
                    classification="libreoffice_timeout",
                    features=(
                        ("auto-long-unwrapped", "latin-text")
                        if index == 0
                        else ("latin-text",)
                    ),
                    status="error",
                )
                for index in range(MODULE.LANES[profile]["parity-a"])
            ]
            report = _report(
                rows,
                profile=profile,
                label="parity-a",
            )
            with self.subTest(profile=profile):
                with self.assertRaisesRegex(
                    MODULE.SummaryError,
                    "workbook_contract",
                ):
                    MODULE._validate_report(
                        report,
                        profile=profile,
                        label="parity-a",
                        shard=None,
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
            (
                lambda value: value["configuration"]["metric_policy"][
                    "unique_text_geometry"
                ]["histogram"].__setitem__(
                    "rounding", "nearest_width_multiple"
                ),
                "metric_policy",
            ),
            (
                lambda value: value["configuration"]["metric_policy"][
                    "unique_text_geometry"
                ].__setitem__(
                    "max_histogram_buckets_per_report", 50_001
                ),
                "metric_policy",
            ),
            (
                lambda value: value["configuration"]["metric_policy"][
                    "unique_text_geometry"
                ].__setitem__("diagnostic_only", 1),
                "metric_policy",
            ),
            (
                lambda value: value["discovery"].__setitem__(
                    "shard_count", True
                ),
                "discovery_merged",
            ),
            (
                lambda value: value["discovery"].__setitem__(
                    "shard_index", False
                ),
                "discovery_merged",
            ),
            (
                lambda value: value["discovery"].__setitem__(
                    "truncated", 0
                ),
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

    def test_preidentity_skips_are_sanitized_without_sha256(self) -> None:
        for classification, status in (
            ("corpus_input_budget_exceeded", "skipped"),
            ("input_limit", "skipped"),
            ("manifest_size_mismatch", "error"),
            ("missing_input", "skipped"),
            ("symlink_input", "skipped"),
            ("unreadable_input", "skipped"),
        ):
            with (
                self.subTest(
                    classification=classification,
                    status=status,
                ),
                tempfile.TemporaryDirectory() as raw,
            ):
                hosted = Path(raw)
                rows = _pilot_rows()
                row = rows[1]
                row["classification"] = classification
                row["status"] = status
                row.pop("sha256")
                row.pop("metrics")
                row.pop("pages")
                _write(
                    hosted / "parity-report-a.json",
                    _report(
                        rows,
                        profile="pilot",
                        label="parity-a",
                    ),
                )
                summary = MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )
                self.assertEqual(
                    summary["reports"][1]["workbooks"],
                    40,
                )

        rows = _pilot_rows()
        rows[1].pop("sha256")
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            with self.assertRaisesRegex(
                MODULE.SummaryError,
                "workbook_contract",
            ):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

    def test_text_geometry_report_budget_is_aggregate_across_rows(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(
                    _pilot_rows(),
                    profile="pilot",
                    label="parity-a",
                ),
            )
            with (
                mock.patch.object(
                    MODULE,
                    "MAX_TEXT_GEOMETRY_REPORT_PAGES",
                    39,
                ),
                self.assertRaisesRegex(
                    MODULE.SummaryError,
                    "text_geometry_report_limit",
                ),
            ):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

        rows = _pilot_rows()
        for row in rows[1:3]:
            _with_geometry(
                row,
                [
                    _with_unique_text_geometry(
                        _geometry_page(),
                        word_histogram=((0, 1),),
                        line_histogram=((0, 1),),
                    )
                ],
            )
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-report-a.json",
                _report(rows, profile="pilot", label="parity-a"),
            )
            with (
                mock.patch.object(
                    MODULE,
                    "MAX_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
                    31,
                ),
                self.assertRaisesRegex(
                    MODULE.SummaryError,
                    "text_geometry_report_limit",
                ),
            ):
                MODULE.summarize(
                    hosted,
                    profile="pilot",
                    baseline_mode="verify",
                    head_sha=HEAD_SHA,
                )

    def test_sharded_geometry_budget_is_partitioned_per_fragment(self) -> None:
        page_rows = [_row(index) for index in range(3)]
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-a-shard-0.json",
                _report(
                    page_rows,
                    profile="full",
                    label="parity-a",
                    shard_index=0,
                ),
            )
            with (
                mock.patch.object(
                    MODULE,
                    "MAX_TEXT_GEOMETRY_REPORT_PAGES",
                    8,
                ),
                self.assertRaisesRegex(
                    MODULE.SummaryError,
                    r"\Atext_geometry_report_limit\Z",
                ) as raised,
            ):
                MODULE.summarize(
                    hosted,
                    profile="full",
                    baseline_mode="candidate",
                    head_sha=HEAD_SHA,
                )
            self.assertNotIn("/srv/private", str(raised.exception))

        bucket_rows = [_row(100 + index) for index in range(2)]
        for row in bucket_rows:
            _with_unique_text_geometry(
                row["pages"][0],
                word_histogram=((0, 1),),
                line_histogram=((0, 1),),
            )
        with tempfile.TemporaryDirectory() as raw:
            hosted = Path(raw)
            _write(
                hosted / "parity-a-shard-0.json",
                _report(
                    bucket_rows,
                    profile="full",
                    label="parity-a",
                    shard_index=0,
                ),
            )
            with (
                mock.patch.object(
                    MODULE,
                    "MAX_TEXT_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
                    64,
                ),
                self.assertRaisesRegex(
                    MODULE.SummaryError,
                    r"\Atext_geometry_report_limit\Z",
                ) as raised,
            ):
                MODULE.summarize(
                    hosted,
                    profile="full",
                    baseline_mode="candidate",
                    head_sha=HEAD_SHA,
                )
            self.assertNotIn("/srv/private", str(raised.exception))

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
                self.assertEqual(result, 0)
                self.assertEqual(
                    stderr.getvalue(),
                    "render-oracle-failure-summary: "
                    "unsafe_or_incomplete_reports_rejected\n",
                )
                self.assertTrue(output.is_file())
                summary = json.loads(output.read_text(encoding="utf-8"))
                self.assertEqual(
                    summary["ingestion"],
                    {
                        "expected_workbooks": 44,
                        "received_workbooks": 0,
                        "status": "rejected",
                    },
                )
                MODULE._validate_output(summary)
                self.assertNotIn(str(root), stderr.getvalue())
                self.assertNotIn(
                    str(root), output.read_text(encoding="utf-8")
                )

    def test_oversized_report_emits_fixed_rejected_summary(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            hosted = root / "hosted"
            hosted.mkdir()
            report = hosted / "parity-report-a.json"
            with report.open("wb") as output:
                output.seek(MODULE.MAX_REPORT_BYTES)
                output.write(b"\n")
            summary_path = root / MODULE.OUTPUT_NAME
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
                        str(summary_path),
                    )
                )

            self.assertEqual(result, 0)
            self.assertEqual(
                stderr.getvalue(),
                "render-oracle-failure-summary: "
                "unsafe_or_incomplete_reports_rejected\n",
            )
            summary = json.loads(
                summary_path.read_text(encoding="utf-8")
            )
            self.assertEqual(
                summary["ingestion"]["status"], "rejected"
            )
            self.assertLessEqual(
                summary_path.stat().st_size, MODULE.MAX_OUTPUT_BYTES
            )
            MODULE._validate_output(summary)

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

            fifo_root = root / "fifo"
            fifo_root.mkdir()
            os.mkfifo(fifo_root / "parity-report-a.json")
            with self.assertRaisesRegex(
                MODULE.SummaryError,
                "report_type_or_size",
            ):
                MODULE.summarize(
                    fifo_root,
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
        page_box_injected = copy.deepcopy(summary)
        page_box_injected["reports"][1]["page_box_geometry"][
            "private_path"
        ] = "/private/workbook.xlsx"
        page_box_policy_drift = copy.deepcopy(summary)
        page_box_policy_drift["reports"][1]["page_box_geometry"][
            "delta_direction"
        ] = "libreoffice_minus_rxls"
        page_box_format_drift = copy.deepcopy(summary)
        page_box_format_drift["reports"][1]["page_box_geometry"][
            "by_format"
        ].pop("ods")
        page_box_feature_injected = copy.deepcopy(summary)
        page_box_feature_injected["reports"][1]["page_box_geometry"][
            "by_feature"
        ]["private-feature"] = copy.deepcopy(
            page_box_feature_injected["reports"][1][
                "page_box_geometry"
            ]["by_format"]["xlsx"]
        )
        page_box_axis_injected = copy.deepcopy(summary)
        page_box_axis_injected["reports"][1]["page_box_geometry"][
            "all"
        ]["by_axis"]["width"]["private_delta"] = 0
        for document in (
            injected,
            drifted,
            unreviewed_stage,
            format_conflict,
            feature_conflict,
            geometry_injected,
            delta_injected,
            geometry_drift,
            page_box_injected,
            page_box_policy_drift,
            page_box_format_drift,
            page_box_feature_injected,
            page_box_axis_injected,
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
