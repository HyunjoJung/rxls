#!/usr/bin/env python3
"""Tests for sanitized Render Oracle failure diagnostics."""

from __future__ import annotations

from collections import Counter
import copy
from contextlib import redirect_stderr
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
                "rxls.render-oracle-failure-summary.v2",
            )
            self.assertEqual(parity["by_format"]["xlsx"]["workbooks"], 10)
            self.assertEqual(
                parity["by_feature"]["korean-text"]["workbooks"], 20
            )
            self.assertEqual(summary["reports"][2], MODULE._empty("parity-b"))

            rendered = payload.decode("utf-8")
            self.assertNotIn("/srv/private", rendered)
            self.assertNotIn("private workbook content", rendered)
            self.assertNotIn('"commands"', rendered)
            self.assertNotIn('"path"', rendered)
            self.assertNotIn('"sha256"', rendered)

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
        for document in (
            injected,
            drifted,
            unreviewed_stage,
            format_conflict,
            feature_conflict,
        ):
            with self.subTest(document=document):
                with self.assertRaises(MODULE.SummaryError):
                    MODULE._validate_output(document)

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
