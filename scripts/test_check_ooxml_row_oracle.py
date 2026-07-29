#!/usr/bin/env python3
"""Tests for the privacy-safe OOXML row diagnostic reducer."""

from __future__ import annotations

from collections import Counter
import copy
from hashlib import sha256
import importlib.util
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts" / "check-ooxml-row-oracle.py"
GENERATOR = ROOT / "scripts" / "generate-ooxml-row-oracle.py"
PARITY_HARNESS = ROOT / "scripts" / "libreoffice-render-parity.py"
LOCAL = ROOT / "local"


def _load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def _json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


class OoxmlRowOracleReducerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.checker = _load(CHECKER, "rxls_check_ooxml_row_oracle")
        cls.generator = _load(GENERATOR, "rxls_generate_ooxml_row_oracle_reducer")

    def setUp(self) -> None:
        self.manifest, _ = self.generator.materialize()
        self.manifest_payload = self.generator._json_bytes(self.manifest)
        _, binding = self.checker._validate_manifest(
            self.manifest, self.manifest_payload
        )
        toolchain = {
            "host_tools_identity_sha256": "1" * 64,
            "kind": "poppler",
            "pdffonts_sha256": "2" * 64,
            "pdfinfo_sha256": "3" * 64,
            "pdftoppm_sha256": "4" * 64,
            "pdftotext_sha256": "5" * 64,
        }
        font_pack_sha256 = "6" * 64
        self.report = {
            "configuration": {
                "caps": {},
                "dpi": 96,
                "font_pack": {
                    "alias_count": 2,
                    "attestation_required": True,
                    "configured": True,
                    "font_count": 3,
                    "fonts_conf_sha256": "7" * 64,
                    "license": "OFL-1.1",
                    "pack_sha256": font_pack_sha256,
                    "pdf_identities_sha256": "8" * 64,
                    "pdf_identity_count": 3,
                },
                "lane_filter": {
                    "formats": ["xlsx"],
                    "required_features": ["ooxml-implicit-row"],
                },
                "locale": "en_US.UTF-8",
                "manifest_binding": binding,
                "measurement_toolchain": toolchain,
                "metric_policy": {
                    "contract_schema": "rxls.render-parity-metrics.v2",
                    "contract_version": 2,
                    "semantic_content_retained": False,
                    "text_box_content_retained": False,
                    "unique_text_geometry": copy.deepcopy(
                        self.checker.UNIQUE_GEOMETRY_POLICY
                    ),
                },
                "min_similarity_ppm": None,
                "oracle_lock": {
                    "build_contract_sha256": "9" * 64,
                    "font_pack_sha256": font_pack_sha256,
                    "image": {
                        "architecture": "linux/amd64",
                        "config_digest": "sha256:" + "a" * 64,
                        "expected_config_digest": "sha256:" + "a" * 64,
                        "expected_manifest_digest": "sha256:" + "b" * 64,
                        "identity_status": "pinned_match",
                        "manifest_digest": "sha256:" + "b" * 64,
                    },
                    "libreoffice": {
                        "artifact_sha256": (
                            self.checker.LIBREOFFICE_ARTIFACT_SHA256
                        ),
                        "name": "LibreOffice",
                        "version": "26.2.3.2",
                    },
                    "lock_file_sha256": "c" * 64,
                    "pdf_font_inspector": copy.deepcopy(toolchain),
                    "runtime": "docker",
                    "schema": "rxls.render-oracle-container-identity.v2",
                },
                "print_mode": "single-page-sheets",
                "renderer_binary": {"bytes": 123_456, "sha256": "d" * 64},
            },
            "discovery": {
                "candidate_count": 24,
                "pre_shard_selected_count": 24,
                "selected_count": 24,
                "shard_candidate_count": 24,
                "shard_count": 1,
                "shard_index": 0,
                "truncated": False,
            },
            "files": [],
            "mode": "compare",
            "preflight": {},
            "schema": "rxls.libreoffice-render-parity.v1",
            "summary": {
                "authored_print": None,
                "by_classification": {"within_threshold": 24},
                "by_status": {"compared": 24},
                "files": 24,
                "input_bytes_considered": self.manifest["total_bytes"],
                "metric_cohorts": {},
            },
        }
        for index, row in enumerate(self.manifest["files"]):
            rxls_height = 792_000 + index
            libreoffice_height = 791_960 + index
            self.report["files"].append(
                {
                    "artifacts": {"libreoffice_pages": 1, "rxls_pages": 1},
                    "bytes": row["byte_length"],
                    "classification": "within_threshold",
                    "commands": [
                        {
                            "returncode": 0,
                            "stderr_nonempty": False,
                            "stdout_nonempty": False,
                        }
                    ],
                    "features": row["features"],
                    "format": "xlsx",
                    "metrics": {"similarity_ppm": 123},
                    "pages": [
                        self._page(rxls_height, libreoffice_height)
                    ],
                    "path": row["path"],
                    "rights_tier": "S",
                    "status": "compared",
                }
            )

    @staticmethod
    def _point(value: int) -> str:
        return f"{value}/1000"

    def _unique_geometry(
        self,
        *,
        rxls_unique: int,
        libreoffice_unique: int,
        matched: int,
        delta_offset: int,
    ) -> dict[str, object]:
        histograms: dict[str, list[dict[str, int]]] = {}
        summaries: dict[str, dict[str, int | None]] = {}
        del delta_offset
        magnitudes = {
            "x_min": 500,
            "x_max": 1_000,
            "y_min": 500,
            "y_max": 1_000,
            "center_x": 750,
            "center_y": 750,
            "width": 500,
            "height": 500,
        }
        for axis in self.checker.UNIQUE_GEOMETRY_AXES:
            magnitude = magnitudes[axis]
            values = (
                [-magnitude, *([magnitude] * (matched - 1))]
                if matched > 0
                else []
            )
            bucket_counts = Counter(
                self.checker._unique_geometry_bucket(value)
                for value in values
            )
            histograms[axis] = [
                {"delta_millipoints": bucket, "count": count}
                for bucket, count in sorted(bucket_counts.items())
            ]
            summaries[axis] = {
                "count": matched,
                "max_delta_millipoints": max(values) if values else None,
                "min_delta_millipoints": min(values) if values else None,
                "negative_overflow_items": 0,
                "positive_overflow_items": 0,
                "sum_delta_millipoints": sum(values),
            }
        return {
            "rxls_unique_items": rxls_unique,
            "libreoffice_unique_items": libreoffice_unique,
            "matched_items": matched,
            "delta_histograms_millipoints": histograms,
            "exact_delta_summaries_millipoints": summaries,
        }

    def _page(self, rxls_height: int, libreoffice_height: int) -> dict[str, object]:
        def side(height: int) -> dict[str, object]:
            dimensions = {
                "height_points": self._point(height),
                "width_points": "612/1",
            }
            return {
                "crop_box": copy.deepcopy(dimensions),
                "media_box": copy.deepcopy(dimensions),
                "page_size": copy.deepcopy(dimensions),
            }

        delta = rxls_height - libreoffice_height
        return {
            "changed_pixels": 1,
            "pdf_point_geometry": {
                "deltas_points": {
                    "crop_box_height": self._point(delta),
                    "crop_box_width": "0/1",
                    "libreoffice_xhtml_page_size_height": "0/1",
                    "libreoffice_xhtml_page_size_width": "0/1",
                    "media_box_height": self._point(delta),
                    "media_box_width": "0/1",
                    "rxls_xhtml_page_size_height": "0/1",
                    "rxls_xhtml_page_size_width": "0/1",
                    "xhtml_height": self._point(delta),
                    "xhtml_width": "0/1",
                },
                "libreoffice": side(libreoffice_height),
                "rxls": side(rxls_height),
                "xhtml": {
                    "libreoffice": {
                        "height_points": self._point(libreoffice_height),
                        "width_points": "612/1",
                    },
                    "rxls": {
                        "height_points": self._point(rxls_height),
                        "width_points": "612/1",
                    },
                },
            },
            "text_box_unique_geometry": self._unique_geometry(
                rxls_unique=4,
                libreoffice_unique=3,
                matched=2,
                delta_offset=10,
            ),
            "text_box_rxls_items": 4,
            "text_box_libreoffice_items": 3,
            "text_box_matched_items": 2,
            "text_line_box_unique_geometry": self._unique_geometry(
                rxls_unique=2,
                libreoffice_unique=2,
                matched=1,
                delta_offset=20,
            ),
            "text_line_box_rxls_items": 2,
            "text_line_box_libreoffice_items": 2,
            "text_line_box_matched_items": 1,
        }

    def _reduce(self, report: dict[str, object] | None = None):
        value = self.report if report is None else report
        payload = _json_bytes(value)
        return self.checker.reduce_report(
            value,
            payload,
            self.manifest,
            self.manifest_payload,
        )

    def test_reduces_to_exact_path_and_content_neutral_contract(self) -> None:
        output = self._reduce()
        self.assertEqual(output["schema"], "rxls.ooxml-row-oracle.v3")
        self.assertIs(output["passed"], True)
        self.assertEqual(
            output["baseline"],
            {
                "case_count": 12,
                "max_absolute_height_delta_millipoints": 40,
                "passed": True,
                "threshold_max_absolute_height_delta_millipoints": 50,
            },
        )
        self.assertEqual(output["coverage"]["case_count"], 24)
        self.assertEqual(output["coverage"]["page_count"], 24)
        self.assertEqual(
            output["geometry_policy"],
            self.checker.UNIQUE_GEOMETRY_POLICY,
        )
        self.assertEqual(
            output["coverage"]["sheet_format_counts"],
            {"missing": 20, "present": 4},
        )
        self.assertEqual(
            output["coverage"]["normal_font_counts"],
            {"carlito": 4, "noto": 20},
        )
        self.assertEqual(
            output["coverage"]["normal_size_point_counts"],
            {"11": 20, "12": 4},
        )
        self.assertEqual(
            output["coverage"]["toggle_counts"],
            self.checker.EXPECTED_TOGGLE_COUNTS,
        )
        self.assertEqual(len(output["cohorts"]), 24)
        self.assertTrue(
            all(row["height_delta_millipoints"] == 40 for row in output["cohorts"])
        )
        for row in output["cohorts"]:
            self.assertEqual(
                set(row["unique_word_geometry"]),
                {
                    "rxls_unique_items",
                    "libreoffice_unique_items",
                    "matched_items",
                    "delta_histograms_millipoints",
                    "exact_delta_summaries_millipoints",
                },
            )
            self.assertEqual(row["unique_word_geometry"]["matched_items"], 2)
            self.assertEqual(row["unique_line_geometry"]["matched_items"], 1)
            self.assertEqual(
                tuple(
                    row["unique_word_geometry"][
                        "delta_histograms_millipoints"
                    ]
                ),
                self.checker.UNIQUE_GEOMETRY_AXES,
            )
            self.assertEqual(
                tuple(
                    row["unique_word_geometry"][
                        "exact_delta_summaries_millipoints"
                    ]
                ),
                self.checker.UNIQUE_GEOMETRY_AXES,
            )
        encoded = _json_bytes(output).decode()
        for forbidden in (
            "payload/",
            ".xlsx",
            "row-missing",
            "row oracle",
            "commands",
            "similarity_ppm",
            "text_box_unique_geometry",
            "text_line_box_unique_geometry",
        ):
            self.assertNotIn(forbidden, encoded)

    def test_rejects_regressed_accepted_baseline(self) -> None:
        report = copy.deepcopy(self.report)
        baseline_index = next(
            index
            for index, row in enumerate(self.manifest["files"])
            if "auto-" not in " ".join(row["features"])
        )
        page = report["files"][baseline_index]["pages"][0]
        geometry = page["pdf_point_geometry"]
        rxls_height = 792_000
        libreoffice_height = 791_949
        geometry["rxls"]["media_box"]["height_points"] = self._point(
            rxls_height
        )
        geometry["libreoffice"]["media_box"]["height_points"] = self._point(
            libreoffice_height
        )
        geometry["deltas_points"]["media_box_height"] = self._point(51)
        with self.assertRaisesRegex(
            self.checker.DiagnosticError,
            "baseline_height_delta",
        ):
            self._reduce(report)

    def test_automatic_height_residual_is_diagnostic_not_a_gate(self) -> None:
        report = copy.deepcopy(self.report)
        automatic_index = next(
            index
            for index, row in enumerate(self.manifest["files"])
            if any(feature.startswith("auto-") for feature in row["features"])
        )
        page = report["files"][automatic_index]["pages"][0]
        geometry = page["pdf_point_geometry"]
        libreoffice_height = 791_960 + automatic_index
        rxls_height = libreoffice_height + 18_020
        geometry["rxls"]["media_box"]["height_points"] = self._point(
            rxls_height
        )
        geometry["deltas_points"]["media_box_height"] = self._point(18_020)
        output = self._reduce(report)
        automatic = next(
            row
            for row in output["cohorts"]
            if row["dimensions"]["toggle"].startswith("auto_")
            and row["height_delta_millipoints"] == 18_020
        )
        self.assertEqual(
            automatic["height_delta_millipoints"],
            18_020,
        )
        self.assertIs(output["passed"], True)
        self.assertIs(output["baseline"]["passed"], True)

    def test_output_is_deterministic_under_report_file_reordering(self) -> None:
        first = self._reduce()
        reordered = copy.deepcopy(self.report)
        reordered["files"].reverse()
        second = self._reduce(reordered)
        self.assertEqual(first["cohorts"], second["cohorts"])
        self.assertEqual(first["coverage"], second["coverage"])
        first_identities = copy.deepcopy(first["identities"])
        second_identities = copy.deepcopy(second["identities"])
        first_identities.pop("report_sha256")
        second_identities.pop("report_sha256")
        self.assertEqual(first_identities, second_identities)

    def test_rejects_swapped_feature_identity(self) -> None:
        report = copy.deepcopy(self.report)
        report["files"][0]["features"], report["files"][1]["features"] = (
            report["files"][1]["features"],
            report["files"][0]["features"],
        )
        with self.assertRaisesRegex(
            self.checker.DiagnosticError, "report_file_contract"
        ):
            self._reduce(report)

    def test_rejects_height_delta_not_derived_from_boxes(self) -> None:
        report = copy.deepcopy(self.report)
        report["files"][0]["pages"][0]["pdf_point_geometry"]["deltas_points"][
            "media_box_height"
        ] = "2/1"
        with self.assertRaisesRegex(
            self.checker.DiagnosticError, "height_delta_identity"
        ):
            self._reduce(report)

    def test_rejects_geometry_that_is_not_exact_millipoints(self) -> None:
        report = copy.deepcopy(self.report)
        geometry = report["files"][0]["pages"][0]["pdf_point_geometry"]
        geometry["rxls"]["media_box"]["height_points"] = "1584001/2000"
        geometry["deltas_points"]["media_box_height"] = "81/2000"
        with self.assertRaisesRegex(
            self.checker.DiagnosticError, "non_integral_millipoints"
        ):
            self._reduce(report)

    def test_rejects_missing_or_malformed_unique_geometry_contract(self) -> None:
        mutations = (
            lambda value: value["files"][0]["pages"][0].pop(
                "text_box_unique_geometry"
            ),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ].update({"unexpected": 1}),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ].update({"rxls_unique_items": True}),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ].update({"matched_items": 4}),
            lambda value: value["files"][0]["pages"][0][
                "text_line_box_unique_geometry"
            ]["delta_histograms_millipoints"].pop("height"),
            lambda value: value["files"][0]["pages"][0][
                "text_line_box_unique_geometry"
            ]["delta_histograms_millipoints"].update({"depth": []}),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ]["delta_histograms_millipoints"]["x_min"][0].update(
                {"unexpected": 1}
            ),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ]["exact_delta_summaries_millipoints"].pop("x_min"),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ]["exact_delta_summaries_millipoints"].update(
                {"depth": copy.deepcopy(
                    value["files"][0]["pages"][0][
                        "text_box_unique_geometry"
                    ]["exact_delta_summaries_millipoints"]["x_min"]
                )}
            ),
            lambda value: value["files"][0]["pages"][0].update(
                {"text_box_rxls_items": 3}
            ),
            lambda value: value["files"][0]["pages"][0].update(
                {"text_box_libreoffice_items": 2}
            ),
            lambda value: value["files"][0]["pages"][0].update(
                {"text_box_matched_items": 1}
            ),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                report = copy.deepcopy(self.report)
                mutation(report)
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError,
                    "(?:text_box|text_line_box)_unique_geometry",
                ):
                    self._reduce(report)

    def test_rejects_unique_geometry_metric_policy_drift(self) -> None:
        mutations = (
            lambda value: value["configuration"]["metric_policy"].pop(
                "unique_text_geometry"
            ),
            lambda value: value["configuration"]["metric_policy"][
                "unique_text_geometry"
            ].update({"diagnostic_only": False}),
            lambda value: value["configuration"]["metric_policy"][
                "unique_text_geometry"
            ]["histogram"].update(
                {"middle_bucket_width_millipoints": 251}
            ),
            lambda value: value["configuration"]["metric_policy"][
                "unique_text_geometry"
            ].update({"max_geometry_pages_per_report": 2_001}),
            lambda value: value["configuration"]["metric_policy"][
                "unique_text_geometry"
            ].update({"unexpected": True}),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                report = copy.deepcopy(self.report)
                mutation(report)
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError,
                    "metric_policy_unique_text_geometry",
                ):
                    self._reduce(report)

    def test_rejects_aggregate_unique_geometry_report_budget(self) -> None:
        with (
            mock.patch.object(
                self.checker,
                "MAX_UNIQUE_GEOMETRY_REPORT_PAGES",
                11,
            ),
            self.assertRaisesRegex(
                self.checker.DiagnosticError,
                "unique_geometry_report_limit",
            ),
        ):
            self._reduce()

        bucket_count = sum(
            len(
                page[key]["delta_histograms_millipoints"][axis]
            )
            for row in self.report["files"]
            for page in row["pages"]
            for key in (
                "text_box_unique_geometry",
                "text_line_box_unique_geometry",
            )
            for axis in self.checker.UNIQUE_GEOMETRY_AXES
        )
        with (
            mock.patch.object(
                self.checker,
                "MAX_UNIQUE_GEOMETRY_REPORT_HISTOGRAM_BUCKETS",
                bucket_count - 1,
            ),
            self.assertRaisesRegex(
                self.checker.DiagnosticError,
                "unique_geometry_report_limit",
            ),
        ):
            self._reduce()

    def test_rejects_malformed_exact_geometry_summaries(self) -> None:
        def summary(
            value: dict[str, object], axis: str = "x_min"
        ) -> dict[str, object]:
            return value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ]["exact_delta_summaries_millipoints"][axis]

        def impossible_bucket_sum(value: dict[str, object]) -> None:
            geometry = value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ]
            geometry["delta_histograms_millipoints"]["x_min"] = [
                {"delta_millipoints": -500, "count": 1},
                {"delta_millipoints": 500, "count": 1},
            ]
            summary(value).update(
                {
                    "max_delta_millipoints": 749,
                    "min_delta_millipoints": -3,
                    "sum_delta_millipoints": 1_498,
                }
            )

        def impossible_axis_identity(value: dict[str, object]) -> None:
            geometry = value["files"][0]["pages"][0][
                "text_line_box_unique_geometry"
            ]
            for axis in ("x_min", "x_max", "width"):
                geometry["delta_histograms_millipoints"][axis] = [
                    {"delta_millipoints": 0, "count": 1}
                ]
                geometry["exact_delta_summaries_millipoints"][axis] = {
                    "count": 1,
                    "max_delta_millipoints": 0,
                    "min_delta_millipoints": 0,
                    "negative_overflow_items": 0,
                    "positive_overflow_items": 0,
                    "sum_delta_millipoints": 0,
                }
            geometry["delta_histograms_millipoints"]["center_x"] = [
                {"delta_millipoints": 1, "count": 1}
            ]
            geometry["exact_delta_summaries_millipoints"]["center_x"] = {
                "count": 1,
                "max_delta_millipoints": 1,
                "min_delta_millipoints": 1,
                "negative_overflow_items": 0,
                "positive_overflow_items": 0,
                "sum_delta_millipoints": 1,
            }

        mutations = (
            lambda value: summary(value).update({"unexpected": 1}),
            lambda value: summary(value).update({"count": 1}),
            lambda value: summary(value).update(
                {"min_delta_millipoints": True}
            ),
            lambda value: summary(value).update(
                {"max_delta_millipoints": 1_000_000_001}
            ),
            lambda value: summary(value).update(
                {
                    "min_delta_millipoints": 250,
                    "max_delta_millipoints": -250,
                }
            ),
            lambda value: summary(value).update(
                {"sum_delta_millipoints": 2_000_000_001}
            ),
            lambda value: summary(value).update(
                {"sum_delta_millipoints": 1_501}
            ),
            lambda value: summary(value).update(
                {
                    "negative_overflow_items": 2,
                    "positive_overflow_items": 1,
                }
            ),
            lambda value: summary(value).update(
                {"min_delta_millipoints": -800}
            ),
            impossible_bucket_sum,
            impossible_axis_identity,
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                report = copy.deepcopy(self.report)
                mutation(report)
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError,
                    "(?:text_box|text_line_box)_unique_geometry_exact_summary",
                ):
                    self._reduce(report)

    def test_rejects_unordered_unbounded_or_unequal_geometry_buckets(self) -> None:
        def word_histogram(value: dict[str, object]) -> dict[str, object]:
            return value["files"][0]["pages"][0]["text_box_unique_geometry"][
                "delta_histograms_millipoints"
            ]

        mutations = (
            lambda value: word_histogram(value)["x_min"][0].update(
                {"delta_millipoints": -1_000_000_001}
            ),
            lambda value: word_histogram(value)["x_max"][1].update(
                {"delta_millipoints": 1_000_000_001}
            ),
            lambda value: word_histogram(value)["y_min"].reverse(),
            lambda value: word_histogram(value)["y_max"][1].update(
                {
                    "delta_millipoints": word_histogram(value)["y_max"][0][
                        "delta_millipoints"
                    ]
                }
            ),
            lambda value: word_histogram(value)["center_x"][0].update(
                {"delta_millipoints": False}
            ),
            lambda value: word_histogram(value)["center_y"][0].update(
                {"count": 0}
            ),
            lambda value: word_histogram(value)["width"][0].update(
                {"count": 2}
            ),
            lambda value: value["files"][0]["pages"][0][
                "text_box_unique_geometry"
            ].update({"libreoffice_unique_items": 250_001}),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                report = copy.deepcopy(self.report)
                mutation(report)
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError,
                    "text_box_unique_geometry",
                ):
                    self._reduce(report)

    def test_accepts_only_the_bounded_bucket_universe_and_attested_overflow(
        self,
    ) -> None:
        self.assertEqual(len(self.checker.UNIQUE_GEOMETRY_BUCKETS), 21)
        report = copy.deepcopy(self.report)
        geometry = report["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ]
        geometry["delta_histograms_millipoints"]["x_min"] = [
            {"delta_millipoints": -12_000, "count": 1},
            {"delta_millipoints": 12_000, "count": 1},
        ]
        geometry["exact_delta_summaries_millipoints"]["x_min"] = {
            "count": 2,
            "max_delta_millipoints": 1_000_000_000,
            "min_delta_millipoints": -1_000_000_000,
            "negative_overflow_items": 1,
            "positive_overflow_items": 1,
            "sum_delta_millipoints": 0,
        }
        output = self._reduce(report)
        overflow_rows = [
            row["unique_word_geometry"][
                "exact_delta_summaries_millipoints"
            ]["x_min"]
            for row in output["cohorts"]
            if row["unique_word_geometry"][
                "exact_delta_summaries_millipoints"
            ]["x_min"]["negative_overflow_items"]
        ]
        self.assertEqual(len(overflow_rows), 1)
        overflow = overflow_rows[0]
        self.assertEqual(overflow["negative_overflow_items"], 1)
        self.assertEqual(overflow["positive_overflow_items"], 1)

        unsupported = copy.deepcopy(report)
        unsupported["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ]["delta_histograms_millipoints"]["x_min"][0][
            "delta_millipoints"
        ] = -11_999
        with self.assertRaisesRegex(
            self.checker.DiagnosticError,
            "text_box_unique_geometry_histogram",
        ):
            self._reduce(unsupported)

        unattested = copy.deepcopy(report)
        unattested["files"][0]["pages"][0][
            "text_box_unique_geometry"
        ]["exact_delta_summaries_millipoints"]["x_min"][
            "negative_overflow_items"
        ] = 0
        with self.assertRaisesRegex(
            self.checker.DiagnosticError,
            "text_box_unique_geometry_exact_summary",
        ):
            self._reduce(unattested)

    def test_nonzero_unique_geometry_is_diagnostic_not_a_fidelity_gate(self) -> None:
        output = self._reduce()
        word = output["cohorts"][0]["unique_word_geometry"]
        self.assertLess(word["matched_items"], word["rxls_unique_items"])
        self.assertTrue(
            any(
                bucket["delta_millipoints"] != 0
                for rows in word["delta_histograms_millipoints"].values()
                for bucket in rows
            )
        )
        self.assertIs(output["passed"], True)

    def test_accepts_zero_match_geometry_with_empty_axis_histograms(self) -> None:
        report = copy.deepcopy(self.report)
        geometry = report["files"][0]["pages"][0][
            "text_line_box_unique_geometry"
        ]
        geometry.update(
            {
                "rxls_unique_items": 1,
                "libreoffice_unique_items": 0,
                "matched_items": 0,
                "delta_histograms_millipoints": {
                    axis: [] for axis in self.checker.UNIQUE_GEOMETRY_AXES
                },
                "exact_delta_summaries_millipoints": {
                    axis: {
                        "count": 0,
                        "max_delta_millipoints": None,
                        "min_delta_millipoints": None,
                        "negative_overflow_items": 0,
                        "positive_overflow_items": 0,
                        "sum_delta_millipoints": 0,
                    }
                    for axis in self.checker.UNIQUE_GEOMETRY_AXES
                },
            }
        )
        output = self._reduce(report)
        lines = [
            row["unique_line_geometry"]
            for row in output["cohorts"]
            if row["unique_line_geometry"]["matched_items"] == 0
        ]
        self.assertEqual(len(lines), 1)
        line = lines[0]
        self.assertEqual(line["matched_items"], 0)
        self.assertTrue(
            all(
                not rows
                for rows in line["delta_histograms_millipoints"].values()
            )
        )
        self.assertIs(output["passed"], True)

        malformed = copy.deepcopy(report)
        malformed["files"][0]["pages"][0][
            "text_line_box_unique_geometry"
        ]["exact_delta_summaries_millipoints"]["height"][
            "sum_delta_millipoints"
        ] = 1
        with self.assertRaisesRegex(
            self.checker.DiagnosticError,
            "text_line_box_unique_geometry_exact_summary",
        ):
            self._reduce(malformed)

    def test_rejects_sharding_and_threshold_pollution(self) -> None:
        for mutation in (
            lambda value: value["discovery"].update(
                {"shard_count": 2, "shard_candidate_count": 6}
            ),
            lambda value: value["configuration"].update(
                {"min_similarity_ppm": 900_000}
            ),
        ):
            with self.subTest(mutation=mutation):
                report = copy.deepcopy(self.report)
                mutation(report)
                with self.assertRaises(self.checker.DiagnosticError):
                    self._reduce(report)

    def test_integer_contracts_reject_bool_aliases(self) -> None:
        for key, alias in (("shard_count", True), ("shard_index", False)):
            with self.subTest(scope="discovery", key=key):
                report = copy.deepcopy(self.report)
                report["discovery"][key] = alias
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError,
                    "discovery_contract",
                ):
                    self._reduce(report)

        manifest = copy.deepcopy(self.manifest)
        manifest["schema_version"] = True
        with self.assertRaisesRegex(
            self.checker.DiagnosticError,
            "manifest_contract",
        ):
            self.checker.reduce_report(
                self.report,
                _json_bytes(self.report),
                manifest,
                _json_bytes(manifest),
            )

        for key in ("page_count", "workbook_count"):
            with self.subTest(scope="output_cohort", key=key):
                output = self._reduce()
                output["cohorts"][0][key] = True
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError,
                    "output_cohort_contract",
                ):
                    self.checker._validate_output(output)

    def test_accepts_only_the_harness_canonical_single_page_mode(self) -> None:
        parity = runpy.run_path(str(PARITY_HARNESS))
        self.assertEqual(
            self.checker.PRINT_MODE_SINGLE_PAGE,
            parity["PRINT_MODE_SINGLE_PAGE"],
        )
        report = copy.deepcopy(self.report)
        report["configuration"]["print_mode"] = "single_page"
        with self.assertRaisesRegex(
            self.checker.DiagnosticError, "report_configuration"
        ):
            self._reduce(report)

    def test_rejects_stale_or_unattested_font_pack_state(self) -> None:
        for mutation in (
            lambda value: value["configuration"]["font_pack"].pop("configured"),
            lambda value: value["configuration"]["font_pack"].update(
                {"attestation_required": False}
            ),
        ):
            with self.subTest(mutation=mutation):
                report = copy.deepcopy(self.report)
                mutation(report)
                with self.assertRaisesRegex(
                    self.checker.DiagnosticError, "font_pack_identity"
                ):
                    self._reduce(report)

    def test_rejects_manifest_matrix_or_binding_mutation(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["files"][0]["features"].append("unexpected-feature")
        manifest["files"][0]["features"].sort()
        with self.assertRaises(self.checker.DiagnosticError):
            self.checker.reduce_report(
                self.report,
                _json_bytes(self.report),
                manifest,
                _json_bytes(manifest),
            )

    def test_output_revalidation_rejects_coverage_or_geometry_mutation(self) -> None:
        output = self._reduce()
        mutations = (
            lambda value: value["coverage"]["normal_font_counts"].update(
                {"noto": 7}
            ),
            lambda value: value["baseline"].update(
                {"max_absolute_height_delta_millipoints": 39}
            ),
            lambda value: value["baseline"].update({"passed": False}),
            lambda value: value["baseline"].update(
                {"threshold_max_absolute_height_delta_millipoints": 51}
            ),
            lambda value: value["baseline"].update({"case_count": 12.0}),
            lambda value: value["baseline"].update(
                {"max_absolute_height_delta_millipoints": 40.0}
            ),
            lambda value: value["baseline"].update(
                {
                    "threshold_max_absolute_height_delta_millipoints": (
                        50.0
                    )
                }
            ),
            lambda value: value["cohorts"][0].update(
                {"height_delta_millipoints": 999}
            ),
            lambda value: value["cohorts"][0]["unique_word_geometry"][
                "delta_histograms_millipoints"
            ]["height"][0].update({"count": 2}),
            lambda value: value["cohorts"][0].pop("unique_line_geometry"),
            lambda value: value["cohorts"].reverse(),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                altered = copy.deepcopy(output)
                mutation(altered)
                with self.assertRaises(self.checker.DiagnosticError):
                    self.checker._validate_output(altered)

        policy_mutated = self._reduce()
        policy_mutated["geometry_policy"]["diagnostic_only"] = False
        with self.assertRaisesRegex(
            self.checker.DiagnosticError,
            "output_geometry_policy",
        ):
            self.checker._validate_output(policy_mutated)

    def test_cli_rejects_duplicate_keys_without_creating_output(self) -> None:
        LOCAL.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix="ooxml-row-reducer-", dir=LOCAL
        ) as temporary:
            root = Path(temporary)
            report = root / "report.json"
            manifest = root / "manifest.json"
            output = root / "aggregate.json"
            report.write_text('{"schema":1,"schema":2}\\n', encoding="utf-8")
            manifest.write_bytes(self.manifest_payload)
            result = subprocess.run(
                [
                    sys.executable,
                    str(CHECKER),
                    str(report),
                    "--campaign-manifest",
                    str(manifest),
                    "--output",
                    str(output),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("duplicate_json_key", result.stderr)
            self.assertFalse(output.exists())

    def test_json_reader_is_bounded_regular_and_race_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            document = root / "document.json"
            document.write_bytes(b"{}")
            link = root / "document-link.json"
            link.symlink_to(document)
            with self.assertRaisesRegex(
                self.checker.DiagnosticError, "fixture"
            ):
                self.checker._load_json(link, 64, "fixture")
            with self.assertRaisesRegex(
                self.checker.DiagnosticError, "fixture"
            ):
                self.checker._load_json(root, 64, "fixture")
            fifo = root / "document.fifo"
            self.checker.os.mkfifo(fifo)
            real_open = self.checker.os.open
            nonblocking = self.checker.os.O_NONBLOCK

            def guarded_open(
                path: object, flags: int, *args: object, **kwargs: object
            ) -> int:
                self.assertNotEqual(flags & nonblocking, 0)
                return real_open(path, flags, *args, **kwargs)

            with mock.patch.object(
                self.checker.os, "open", side_effect=guarded_open
            ), self.assertRaisesRegex(
                self.checker.DiagnosticError, "fixture"
            ):
                self.checker._load_json(fifo, 64, "fixture")

            document.write_bytes(b"0123456789")
            real_read = self.checker.os.read
            returned = 0

            def observed_read(descriptor: int, count: int) -> bytes:
                nonlocal returned
                chunk = real_read(descriptor, count)
                returned += len(chunk)
                return chunk

            with mock.patch.object(
                self.checker.os, "read", side_effect=observed_read
            ), self.assertRaisesRegex(
                self.checker.DiagnosticError, "fixture_limit"
            ):
                self.checker._load_json(document, 4, "fixture")
            self.assertEqual(returned, 5)

            for mutation in ("growth", "swap"):
                with self.subTest(mutation=mutation):
                    document.write_bytes(b"{}")
                    replacement = root / "replacement.json"
                    replacement.write_bytes(b"{}")
                    changed = False

                    def adversarial_read(
                        descriptor: int, count: int
                    ) -> bytes:
                        nonlocal changed
                        chunk = real_read(descriptor, count)
                        if chunk and not changed:
                            changed = True
                            if mutation == "growth":
                                document.write_bytes(b"{} ")
                            else:
                                replacement.replace(document)
                        return chunk

                    with mock.patch.object(
                        self.checker.os,
                        "read",
                        side_effect=adversarial_read,
                    ), self.assertRaisesRegex(
                        self.checker.DiagnosticError, "fixture"
                    ):
                        self.checker._load_json(document, 64, "fixture")

    def test_cli_writes_only_validated_aggregate(self) -> None:
        LOCAL.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix="ooxml-row-reducer-", dir=LOCAL
        ) as temporary:
            root = Path(temporary)
            report = root / "report.json"
            manifest = root / "manifest.json"
            output = root / "aggregate.json"
            report_payload = _json_bytes(self.report)
            report.write_bytes(report_payload)
            manifest.write_bytes(self.manifest_payload)
            result = subprocess.run(
                [
                    sys.executable,
                    str(CHECKER),
                    str(report),
                    "--campaign-manifest",
                    str(manifest),
                    "--output",
                    str(output),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            document = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(document, json.loads(result.stdout))
            self.assertEqual(
                document["identities"]["report_sha256"],
                sha256(report_payload).hexdigest(),
            )
            self.checker._validate_output(document)


if __name__ == "__main__":
    unittest.main()
