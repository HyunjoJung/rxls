#!/usr/bin/env python3
"""Tests for the privacy-safe OOXML row diagnostic reducer."""

from __future__ import annotations

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
                "candidate_count": 12,
                "pre_shard_selected_count": 12,
                "selected_count": 12,
                "shard_candidate_count": 12,
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
                "by_classification": {"within_threshold": 12},
                "by_status": {"compared": 12},
                "files": 12,
                "input_bytes_considered": self.manifest["total_bytes"],
                "metric_cohorts": {},
            },
        }
        for index, row in enumerate(self.manifest["files"]):
            rxls_height = 792_000 + index
            libreoffice_height = 791_000 + index
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
        self.assertEqual(output["schema"], "rxls.ooxml-row-oracle.v1")
        self.assertIs(output["passed"], True)
        self.assertEqual(output["coverage"]["case_count"], 12)
        self.assertEqual(output["coverage"]["page_count"], 12)
        self.assertEqual(
            output["coverage"]["sheet_format_counts"],
            {"missing": 8, "present": 4},
        )
        self.assertEqual(
            output["coverage"]["normal_font_counts"],
            {"carlito": 4, "noto": 8},
        )
        self.assertEqual(
            output["coverage"]["normal_size_point_counts"],
            {"11": 8, "12": 4},
        )
        self.assertEqual(
            output["coverage"]["toggle_counts"],
            {
                "explicit_row_height": 1,
                "hidden_row": 1,
                "image_drawing": 1,
                "none": 8,
                "right_to_left_layout": 1,
            },
        )
        self.assertEqual(len(output["cohorts"]), 12)
        self.assertTrue(
            all(row["height_delta_millipoints"] == 1_000 for row in output["cohorts"])
        )
        encoded = _json_bytes(output).decode()
        for forbidden in (
            "payload/",
            ".xlsx",
            "row-missing",
            "row oracle",
            "commands",
            "similarity_ppm",
        ):
            self.assertNotIn(forbidden, encoded)

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
        geometry["deltas_points"]["media_box_height"] = "2001/2000"
        with self.assertRaisesRegex(
            self.checker.DiagnosticError, "non_integral_millipoints"
        ):
            self._reduce(report)

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
            lambda value: value["cohorts"][0].update(
                {"height_delta_millipoints": 999}
            ),
            lambda value: value["cohorts"].reverse(),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                altered = copy.deepcopy(output)
                mutation(altered)
                with self.assertRaises(self.checker.DiagnosticError):
                    self.checker._validate_output(altered)

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
