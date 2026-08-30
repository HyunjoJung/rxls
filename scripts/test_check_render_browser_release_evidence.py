from __future__ import annotations

import base64
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check_render_browser_release_evidence.py"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "render-package-release.yml"
SPEC = importlib.util.spec_from_file_location(
    "check_render_browser_release_evidence", SCRIPT
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

HEAD_SHA = "a" * 40
RUN_ID = 12345
RUN_ATTEMPT = 2


class BrowserEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.archive = self.root / "rxls-render-worker-0.1.3.tgz"
        self.archive.write_bytes(b"deterministic npm archive fixture\n" * 64)
        archive = self.archive.read_bytes()
        integrity = base64.b64encode(hashlib.sha512(archive).digest()).decode("ascii")
        self.pack = self.root / "npm-pack.json"
        self.pack.write_text(
            json.dumps(
                [
                    {
                        "name": "@rxls/render-worker",
                        "version": "0.1.3",
                        "filename": self.archive.name,
                        "size": len(archive),
                        "unpackedSize": len(MODULE.EXPECTED_PACKAGE_FILES),
                        "shasum": hashlib.sha1(archive).hexdigest(),
                        "integrity": f"sha512-{integrity}",
                        "entryCount": len(MODULE.EXPECTED_PACKAGE_FILES),
                        "files": [
                            {"path": path, "size": 1}
                            for path in sorted(MODULE.EXPECTED_PACKAGE_FILES)
                        ],
                    }
                ],
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        self.runtime = self.root / "runtime.txt"
        self.runtime.write_bytes(MODULE.EXPECTED_RUNTIME_TEXT)
        self.source = self.root / "source.log"
        self.installed = self.root / "installed.log"
        self.source.write_text(
            self._pass_line("source", baseline=2_000_000, peak=62_000_000, retained=8_000_000)
            + "\n",
            encoding="utf-8",
        )
        self.installed.write_text(
            self._pass_line(
                "installed",
                baseline=2_500_000,
                peak=25_000_000,
                retained=8_500_000,
            )
            + "\n",
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def _pass_line(
        self,
        mode: str,
        *,
        baseline: int,
        peak: int,
        retained: int,
        rss_baseline: int = 100_000_000,
        rss_peak: int = 500_000_000,
        rss_retained: int = 200_000_000,
        elapsed: int = 550,
        deadline: int = MODULE.HARD_STOP_DEADLINE_MS,
        wasm_url: str | None = None,
        network_error: str = MODULE.EXPECTED_NETWORK_ERROR,
        behavior: dict[str, object] | None = None,
        rss_boundary_interval: int = MODULE.RSS_BOUNDARY_INTERVAL_MS,
        rss_boundary_samples: int = MODULE.RSS_BOUNDARY_REQUIRED_SAMPLES,
        rss_boundary_required: int = MODULE.RSS_BOUNDARY_REQUIRED_SAMPLES,
        rss_boundary_duration: int = 40,
        rss_boundary_max_gap: int = 10,
        rss_boundary_growth: int | None = None,
        rss_boundary_minimum_growth: int = (
            MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES
        ),
        rss_boundary_peak: int | None = None,
        route_sha256: str | None = None,
        csp_sha256: str | None = None,
        network_workers: int = MODULE.NETWORK_PROOF_WORKERS,
        network_requests: int = MODULE.NETWORK_PROOF_REQUESTS,
        pre_navigation: str = "true",
    ) -> str:
        growth = max(0, retained - baseline)
        rss_peak_growth = max(0, rss_peak - rss_baseline)
        rss_retained_growth = max(0, rss_retained - rss_baseline)
        if rss_boundary_growth is None and rss_boundary_peak is None:
            rss_boundary_growth = 128 * 1024 * 1024
            rss_boundary_peak = rss_baseline + rss_boundary_growth
        elif rss_boundary_growth is None:
            rss_boundary_growth = rss_boundary_peak - rss_baseline
        elif rss_boundary_peak is None:
            rss_boundary_peak = rss_baseline + rss_boundary_growth
        if wasm_url is None:
            path = MODULE.EXPECTED_WASM_PATHS[mode]
            wasm_url = f"http://127.0.0.1:43210{path}"
        if behavior is None:
            behavior = self._behavior_proof()
        if route_sha256 is None:
            route_sha256 = ("b" if mode == "source" else "c") * 64
        if csp_sha256 is None:
            csp_sha256 = ("d" if mode == "source" else "e") * 64
        proof = json.dumps(behavior, separators=(",", ":"), sort_keys=True)
        return (
            f"PROOF {proof}\n"
            f"RSS_BOUNDARY interval={rss_boundary_interval}ms "
            f"samples={rss_boundary_samples} "
            f"required={rss_boundary_required} "
            f"duration={rss_boundary_duration}ms "
            f"max-gap={rss_boundary_max_gap}ms "
            f"growth={rss_boundary_growth} "
            f"minimum-growth={rss_boundary_minimum_growth} "
            f"peak={rss_boundary_peak}\n"
            f"NETWORK_PROOF route={route_sha256} csp={csp_sha256} "
            f"workers={network_workers} requests={network_requests} "
            f"pre-nav={pre_navigation}\n"
            "PASS Google Chrome for Testing 150.0.7871.115 "
            f"{MODULE.MODE_DESCRIPTIONS[mode]}; "
            f"heap baseline={baseline} peak={peak} retained={retained} "
            f"growth={growth} bytes; "
            f"rss baseline={rss_baseline} peak={rss_peak} "
            f"peak-growth={rss_peak_growth} retained={rss_retained} "
            f"retained-growth={rss_retained_growth} bytes; "
            f"hard-stop target={elapsed}/{deadline}ms wasm={wasm_url}; "
            f"CSP Network={network_error}"
        )

    def _behavior_proof(self) -> dict[str, object]:
        width_raw = 1024 * 100
        height_raw = 1024 * 200
        return {
            "schema": MODULE.BEHAVIOR_SCHEMA,
            "fixture": {
                "workbookBytes": 1_000,
                "workbookSha256": "1" * 64,
                "fontPackSha256": "2" * 64,
                "renderedImageBytes": 100,
                "renderedImageSha256": "3" * 64,
            },
            "capabilitiesSha256": "4" * 64,
            "cancellation": {
                "abortSignal": "AbortError",
                "activeOpen": "AbortError",
                "reopenedDocument": True,
            },
            "progress": [
                {"completed": 0, "total": 3, "stage": "accepted"},
                {"completed": 1, "total": 3, "stage": "parsing"},
                {"completed": 2, "total": 3, "stage": "finalizing"},
                {"completed": 3, "total": 3, "stage": "complete"},
            ],
            "pendingBoundary": {
                "inputBytes": 32 * 1024 * 1024,
                "queuedRequests": 4,
                "pendingResourceBytes": 128 * 1024 * 1024,
                "overflowBytes": 1,
                "overflowOutcome": {
                    "synchronous": True,
                    "code": "limit_exceeded",
                    "resource": "pendingResourceBytes",
                },
                "rejectedRequests": 4,
                "rejectionCode": "client_closed",
                "dispatchedRequests": 0,
                "transportTerminated": True,
            },
            "limits": {
                "fontFiles": {
                    "code": "limit_exceeded",
                    "resource": "fontFiles",
                },
                "hardPage": {"code": "limit_exceeded", "resource": "pages"},
                "dpi": {"code": "dpi_out_of_range", "resource": None},
                "outputBytes": {
                    "code": "limit_exceeded",
                    "resource": "output_bytes",
                },
                "imageCount": {
                    "code": "limit_exceeded",
                    "resource": "maxImages",
                },
                "imageBytes": {
                    "code": "limit_exceeded",
                    "resource": "maxImageBytes",
                },
            },
            "tile": {
                "firstRow": 0,
                "firstCol": 0,
                "lastRow": 63,
                "lastCol": 31,
                "bytes": 250_000,
                "sha256": "5" * 64,
            },
            "pages": {
                "count": 8,
                "paper": {
                    "widthRaw": width_raw,
                    "heightRaw": height_raw,
                },
                "first": {
                    "pageIndex": 0,
                    "responsePageIndex": 0,
                    "pageMapSha256": "6" * 64,
                    "svg": {
                        "bytes": 1_000,
                        "sha256": "7" * 64,
                        "widthRaw": width_raw,
                        "heightRaw": height_raw,
                    },
                },
                "nonzero": {
                    "pageIndex": 7,
                    "responsePageIndex": 7,
                    "pageMapSha256": "8" * 64,
                    "svg": {
                        "bytes": 1_000,
                        "sha256": "9" * 64,
                        "repeatSha256": "9" * 64,
                        "widthRaw": width_raw,
                        "heightRaw": height_raw,
                    },
                    "png": {
                        "bytes": 1_000,
                        "sha256": "a" * 64,
                        "width": 100,
                        "height": 200,
                        "dpi": 96,
                    },
                },
                "outOfRange": {
                    "pageIndex": 8,
                    "code": "page_index_out_of_range",
                },
            },
            "hardStop": {
                "deadlineMs": MODULE.HARD_STOP_DEADLINE_MS,
                "rejectedRequests": 2,
            },
            "network": {
                "cspNegativeControl": True,
                "unexpectedExternalResources": 0,
            },
        }

    def _summary(self, platform: str = "linux") -> dict[str, object]:
        return MODULE.build_summary(
            source_log=self.source,
            installed_log=self.installed,
            runtime_evidence=self.runtime,
            npm_pack=self.pack,
            npm_archive=self.archive,
            head_sha=HEAD_SHA,
            platform=platform,
            repository=MODULE.EXPECTED_REPOSITORY,
            workflow_run_id=RUN_ID,
            workflow_run_attempt=RUN_ATTEMPT,
        )

    def test_build_and_validate_exact_summary(self) -> None:
        summary = self._summary()
        self.assertEqual(summary["schema"], MODULE.SCHEMA)
        self.assertEqual(summary["head_sha"], HEAD_SHA)
        self.assertEqual(
            summary["package"]["entry_count"],
            len(MODULE.EXPECTED_PACKAGE_FILES),
        )
        self.assertTrue(summary["behavior"]["source_installed_equal"])
        self.assertEqual(
            summary["modes"]["source"]["behavior_sha256"],
            summary["behavior"]["sha256"],
        )
        self.assertEqual(
            summary["modes"]["installed"]["behavior_sha256"],
            summary["behavior"]["sha256"],
        )
        self.assertEqual(
            summary["modes"]["source"]["hard_stop"]["deadline_ms"],
            MODULE.HARD_STOP_DEADLINE_MS,
        )
        self.assertTrue(
            summary["modes"]["source"]["hard_stop"]["wasm_frame_confirmed"]
        )
        self.assertEqual(
            summary["modes"]["source"]["network"]["error_text"],
            MODULE.EXPECTED_NETWORK_ERROR,
        )
        self.assertEqual(
            summary["modes"]["source"]["process_tree_rss"]["peak_bytes"],
            500_000_000,
        )
        self.assertEqual(
            summary["modes"]["source"]["rss_boundary"],
            {
                "baseline_bytes": 100_000_000,
                "duration_ms": 40,
                "growth_bytes": 128 * 1024 * 1024,
                "interval_ms": MODULE.RSS_BOUNDARY_INTERVAL_MS,
                "max_gap_ms": 10,
                "minimum_growth_bytes": (
                    MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES
                ),
                "peak_bytes": 100_000_000 + (128 * 1024 * 1024),
                "process_peak_bound": True,
                "required_samples": MODULE.RSS_BOUNDARY_REQUIRED_SAMPLES,
                "sample_count": MODULE.RSS_BOUNDARY_REQUIRED_SAMPLES,
            },
        )
        self.assertEqual(
            summary["modes"]["source"]["network_proof"],
            {
                "csp_sha256": "d" * 64,
                "pre_navigation": True,
                "request_count": MODULE.NETWORK_PROOF_REQUESTS,
                "route_sha256": "b" * 64,
                "worker_count": MODULE.NETWORK_PROOF_WORKERS,
            },
        )
        self.assertNotEqual(
            summary["modes"]["source"]["network_proof"],
            summary["modes"]["installed"]["network_proof"],
        )
        self.assertEqual(
            MODULE.validate_summary(
                summary,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            ),
            summary,
        )

    def test_false_green_log_with_failure_is_rejected(self) -> None:
        self.source.write_text(
            "FAIL harness: timed out\n"
            + self._pass_line(
                "source", baseline=2_000_000, peak=3_000_000, retained=2_100_000
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_log_failure"):
            self._summary()

    def test_source_and_installed_behavior_must_match_exactly(self) -> None:
        behavior = self._behavior_proof()
        behavior["capabilitiesSha256"] = "b" * 64
        self.installed.write_text(
            self._pass_line(
                "installed",
                baseline=2_500_000,
                peak=25_000_000,
                retained=8_500_000,
                behavior=behavior,
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "behavior_parity"):
            self._summary()

    def test_source_and_installed_network_digests_must_be_distinct(self) -> None:
        cases = (
            {
                "route_sha256": "b" * 64,
                "csp_sha256": "e" * 64,
            },
            {
                "route_sha256": "c" * 64,
                "csp_sha256": "d" * 64,
            },
        )
        for evidence in cases:
            with self.subTest(evidence=evidence):
                self.installed.write_text(
                    self._pass_line(
                        "installed",
                        baseline=2_500_000,
                        peak=25_000_000,
                        retained=8_500_000,
                        **evidence,
                    )
                    + "\n",
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    "network_proof_distinct",
                ):
                    self._summary()

    def test_summary_network_digests_must_be_distinct(self) -> None:
        for field in ("route_sha256", "csp_sha256"):
            with self.subTest(field=field):
                summary = self._summary()
                summary["modes"]["installed"]["network_proof"][field] = summary[
                    "modes"
                ]["source"]["network_proof"][field]
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    "summary_network_proof_distinct",
                ):
                    MODULE.validate_summary(
                        summary,
                        head_sha=HEAD_SHA,
                        platform="linux",
                        repository=MODULE.EXPECTED_REPOSITORY,
                        workflow_run_id=RUN_ID,
                        workflow_run_attempt=RUN_ATTEMPT,
                    )

    def test_pending_boundary_contract_is_exact(self) -> None:
        for field, value in (
            ("pendingResourceBytes", (128 * 1024 * 1024) - 1),
            ("queuedRequests", 3),
            ("dispatchedRequests", 1),
            ("transportTerminated", False),
        ):
            with self.subTest(field=field):
                behavior = self._behavior_proof()
                behavior["pendingBoundary"][field] = value
                self.source.write_text(
                    self._pass_line(
                        "source",
                        baseline=2_000_000,
                        peak=62_000_000,
                        retained=8_000_000,
                        behavior=behavior,
                    )
                    + "\n",
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    "behavior_pending_boundary",
                ):
                    self._summary()

    def test_evidence_lines_are_unique_and_ordered_before_final_pass(self) -> None:
        line = self._pass_line(
            "source", baseline=2_000_000, peak=62_000_000, retained=8_000_000
        )
        lines = line.splitlines()
        variants = {
            "reordered": [lines[0], lines[2], lines[1], lines[3]],
            "duplicate_proof": [
                lines[0],
                lines[0],
                lines[1],
                lines[2],
                lines[3],
            ],
            "duplicate_rss": [lines[0], lines[1], lines[1], lines[2], lines[3]],
            "duplicate_network": [
                lines[0],
                lines[1],
                lines[2],
                lines[2],
                lines[3],
            ],
        }
        for label, variant in variants.items():
            with self.subTest(label=label):
                self.source.write_text("\n".join(variant) + "\n", encoding="utf-8")
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    "source_evidence_lines",
                ):
                    self._summary()

    def test_rss_boundary_metrics_are_bounded_and_process_bound(self) -> None:
        invalid_cases = (
            ("interval", {"rss_boundary_interval": 26}),
            ("samples", {"rss_boundary_samples": 4}),
            ("required", {"rss_boundary_required": 6}),
            ("duration", {"rss_boundary_duration": 2001}),
            ("gap_101ms", {"rss_boundary_duration": 101, "rss_boundary_max_gap": 101}),
            ("gap_relation", {"rss_boundary_duration": 5, "rss_boundary_max_gap": 10}),
        )
        for label, overrides in invalid_cases:
            with self.subTest(label=label):
                self.source.write_text(
                    self._pass_line(
                        "source",
                        baseline=2_000_000,
                        peak=62_000_000,
                        retained=8_000_000,
                        **overrides,
                    )
                    + "\n",
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    "source_rss_boundary",
                ):
                    self._summary()
        materiality_cases = (
            (
                "minimum_drift",
                {
                    "rss_boundary_minimum_growth": (
                        MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES - 1
                    )
                },
            ),
            (
                "below_minimum",
                {
                    "rss_boundary_growth": (
                        MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES - 1
                    )
                },
            ),
            (
                "growth_drift",
                {
                    "rss_boundary_growth": (
                        MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES
                    ),
                    "rss_boundary_peak": (
                        100_000_000
                        + MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES
                        + 1
                    ),
                },
            ),
        )
        for label, overrides in materiality_cases:
            with self.subTest(label=label):
                self.source.write_text(
                    self._pass_line(
                        "source",
                        baseline=2_000_000,
                        peak=62_000_000,
                        retained=8_000_000,
                        **overrides,
                    )
                    + "\n",
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    "source_rss_boundary_materiality",
                ):
                    self._summary()
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=62_000_000,
                retained=8_000_000,
                rss_boundary_peak=500_000_001,
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "source_rss_boundary_peak",
        ):
            self._summary()
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=62_000_000,
                retained=8_000_000,
                rss_boundary_samples=10**20,
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "source_rss_boundary_integer",
        ):
            self._summary()
        for label, overrides, error_code in (
            (
                "unbounded_growth",
                {"rss_boundary_growth": 10**20},
                "source_rss_boundary_integer",
            ),
            (
                "zero_growth",
                {"rss_boundary_growth": 0},
                "source_rss_boundary",
            ),
            (
                "unbounded_minimum",
                {"rss_boundary_minimum_growth": 10**20},
                "source_rss_boundary_integer",
            ),
        ):
            with self.subTest(label=label):
                self.source.write_text(
                    self._pass_line(
                        "source",
                        baseline=2_000_000,
                        peak=62_000_000,
                        retained=8_000_000,
                        **overrides,
                    )
                    + "\n",
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    error_code,
                ):
                    self._summary()
        ordered = self._pass_line(
            "source",
            baseline=2_000_000,
            peak=62_000_000,
            retained=8_000_000,
        )
        reordered = ordered.replace(
            "growth=134217728 minimum-growth=100663296",
            "minimum-growth=100663296 growth=134217728",
        )
        self.source.write_text(reordered + "\n", encoding="utf-8")
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "source_rss_boundary_line",
        ):
            self._summary()

    def test_network_proof_identity_and_digests_fail_closed(self) -> None:
        invalid_cases = (
            ("workers", {"network_workers": 1}, "source_network_proof"),
            ("requests", {"network_requests": 18}, "source_network_proof"),
            (
                "unbounded_workers",
                {"network_workers": 10**20},
                "source_network_proof_integer",
            ),
            ("pre_navigation", {"pre_navigation": "false"}, "source_network_proof"),
            (
                "route_digest",
                {"route_sha256": "g" * 64},
                "source_network_proof_line",
            ),
            (
                "csp_digest",
                {"csp_sha256": "A" * 64},
                "source_network_proof_line",
            ),
        )
        for label, overrides, error_code in invalid_cases:
            with self.subTest(label=label):
                self.source.write_text(
                    self._pass_line(
                        "source",
                        baseline=2_000_000,
                        peak=62_000_000,
                        retained=8_000_000,
                        **overrides,
                    )
                    + "\n",
                    encoding="utf-8",
                )
                with self.assertRaisesRegex(
                    MODULE.BrowserEvidenceError,
                    error_code,
                ):
                    self._summary()

    def test_pass_must_be_unique_and_final(self) -> None:
        line = self._pass_line(
            "source", baseline=2_000_000, peak=3_000_000, retained=2_100_000
        )
        self.source.write_text(f"{line}\ntrailing diagnostic\n", encoding="utf-8")
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_pass_line"):
            self._summary()

    def test_runtime_version_and_mode_are_exact(self) -> None:
        line = self._pass_line(
            "source", baseline=2_000_000, peak=3_000_000, retained=2_100_000
        ).replace("150.0.7871.115", "150.0.7871.116")
        self.source.write_text(line + "\n", encoding="utf-8")
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError, "source_runtime_identity"
        ):
            self._summary()

    def test_heap_identity_and_bounds_fail_closed(self) -> None:
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=300_000_000,
                retained=2_100_000,
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_heap"):
            self._summary()

    def test_hard_stop_deadline_fails_closed(self) -> None:
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=3_000_000,
                retained=2_100_000,
                elapsed=MODULE.HARD_STOP_DEADLINE_MS + 1,
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_hard_stop"):
            self._summary()

    def test_process_tree_rss_identity_and_bounds_fail_closed(self) -> None:
        rss_baseline = 100_000_000
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=3_000_000,
                retained=2_100_000,
                rss_baseline=rss_baseline,
                rss_peak=rss_baseline + (1 << 30) + 1,
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_rss"):
            self._summary()

    def test_process_tree_peak_budget_excludes_the_pre_workload_browser_baseline(
        self,
    ) -> None:
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=3_000_000,
                retained=2_100_000,
                rss_baseline=1_270_398_976,
                rss_peak=1_688_551_424,
                rss_retained=1_519_628_288,
            )
            + "\n",
            encoding="utf-8",
        )
        summary = self._summary()
        process_tree = summary["modes"]["source"]["process_tree_rss"]
        self.assertEqual(process_tree["peak_growth_bytes"], 418_152_448)
        self.assertEqual(process_tree["retained_growth_bytes"], 249_229_312)

    def test_explicit_darwin_platform_uses_only_the_pinned_override(self) -> None:
        for mode, path in (("source", self.source), ("installed", self.installed)):
            rss_baseline = 1_080_000_000
            path.write_text(
                self._pass_line(
                    mode,
                    baseline=2_000_000,
                    peak=60_000_000,
                    retained=8_000_000,
                    rss_baseline=rss_baseline,
                    rss_peak=rss_baseline + (3 << 29),
                    rss_retained=1_470_000_000,
                )
                + "\n",
                encoding="utf-8",
            )
        summary = self._summary("darwin")
        self.assertEqual(summary["platform"], "darwin")
        self.assertEqual(
            summary["limits"]["max_process_tree_peak_growth_bytes"],
            2 << 30,
        )
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError, "installed_rss"
        ):
            self._summary("linux")
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "summary_binding"):
            MODULE.validate_summary(
                summary,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )

    def test_wasm_frame_url_is_mode_bound_and_local(self) -> None:
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=3_000_000,
                retained=2_100_000,
                wasm_url=(
                    "http://127.0.0.1:43210/"
                    "installed-package/pkg/rxls_render_wasm_bg.wasm"
                ),
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_wasm"):
            self._summary()

    def test_network_negative_control_identity_is_exact(self) -> None:
        self.source.write_text(
            self._pass_line(
                "source",
                baseline=2_000_000,
                peak=3_000_000,
                retained=2_100_000,
                network_error="net::ERR_FAILED",
            )
            + "\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "source_network"):
            self._summary()

    def test_runtime_closure_file_is_exact(self) -> None:
        self.runtime.write_text("PASS\n", encoding="utf-8")
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "runtime_evidence"):
            self._summary()

    def test_npm_integrity_and_file_allowlist_are_bound(self) -> None:
        [packed] = json.loads(self.pack.read_text(encoding="utf-8"))
        packed["files"][0]["path"] = "tests/secret.xlsx"
        self.pack.write_text(json.dumps([packed]) + "\n", encoding="utf-8")
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "npm_pack_files"):
            self._summary()

    def test_summary_unknown_fields_and_metric_mutations_are_rejected(self) -> None:
        summary = self._summary()
        extra = copy.deepcopy(summary)
        extra["source_path"] = "/tmp/private.xlsx"
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "summary_fields"):
            MODULE.validate_summary(
                extra,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["heap"]["retained_growth_bytes"] += 1
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "summary_source_heap"):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        process_tree = mutated["modes"]["source"]["process_tree_rss"]
        process_tree["peak_growth_bytes"] += 1
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "summary_source_rss"):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["rss_boundary"]["peak_bytes"] = (
            mutated["modes"]["source"]["process_tree_rss"]["peak_bytes"] + 1
        )
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "summary_source_rss_boundary",
        ):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["rss_boundary"]["growth_bytes"] += 1
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "summary_source_rss_boundary",
        ):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["rss_boundary"][
            "minimum_growth_bytes"
        ] -= 1
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "summary_source_rss_boundary",
        ):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["network_proof"]["request_count"] = 18
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError,
            "summary_source_network_proof",
        ):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["network"]["response_received"] = True
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError, "summary_source_network"
        ):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["behavior"]["sha256"] = "0" * 64
        with self.assertRaisesRegex(
            MODULE.BrowserEvidenceError, "summary_behavior_digest"
        ):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )
        mutated = copy.deepcopy(summary)
        mutated["modes"]["source"]["behavior_sha256"] = "0" * 64
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "summary_source"):
            MODULE.validate_summary(
                mutated,
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )

    def test_authenticated_single_file_artifact_is_release_bound(self) -> None:
        summary_payload = MODULE._canonical_payload(self._summary())
        artifact = self.root / "artifact.zip"
        with zipfile.ZipFile(
            artifact, "w", compression=zipfile.ZIP_DEFLATED
        ) as archive:
            archive.writestr(MODULE.SUMMARY_NAME, summary_payload)
        payload = artifact.read_bytes()
        report = MODULE.validate_artifact(
            artifact,
            artifact_id=987,
            artifact_name=f"render-browser-{HEAD_SHA}-{RUN_ID}-{RUN_ATTEMPT}",
            artifact_size_bytes=len(payload),
            artifact_digest=f"sha256:{hashlib.sha256(payload).hexdigest()}",
            head_sha=HEAD_SHA,
            platform="linux",
            repository=MODULE.EXPECTED_REPOSITORY,
            workflow_run_id=RUN_ID,
            workflow_run_attempt=RUN_ATTEMPT,
        )
        self.assertEqual(report["schema"], MODULE.PREREQUISITE_SCHEMA)
        self.assertTrue(report["passed"])
        self.assertEqual(
            report["browser_evidence_sha256"],
            hashlib.sha256(summary_payload).hexdigest(),
        )
        self.assertEqual(
            report["behavior_sha256"],
            self._summary()["behavior"]["sha256"],
        )
        self.assertEqual(report["behavior_schema"], MODULE.BEHAVIOR_SCHEMA)
        self.assertEqual(
            report["pending_boundary"],
            self._behavior_proof()["pendingBoundary"],
        )
        self.assertEqual(
            report["pending_boundary_sha256"],
            hashlib.sha256(
                MODULE._canonical_payload(report["pending_boundary"])
            ).hexdigest(),
        )
        self.assertEqual(
            report["mode_proofs"]["source"]["network_proof"]["route_sha256"],
            "b" * 64,
        )
        self.assertLessEqual(
            report["mode_proofs"]["source"]["rss_boundary"]["peak_bytes"],
            report["mode_proofs"]["source"]["process_tree_peak_bytes"],
        )
        self.assertEqual(
            report["mode_proofs"]["source"]["rss_boundary"][
                "baseline_bytes"
            ],
            report["mode_proofs"]["source"]["process_tree_baseline_bytes"],
        )
        self.assertEqual(
            report["mode_proofs"]["source"]["rss_boundary"]["growth_bytes"],
            report["mode_proofs"]["source"]["rss_boundary"]["peak_bytes"]
            - report["mode_proofs"]["source"]["rss_boundary"][
                "baseline_bytes"
            ],
        )
        self.assertEqual(
            report["mode_proofs"]["source"]["rss_boundary"][
                "minimum_growth_bytes"
            ],
            MODULE.RSS_BOUNDARY_MINIMUM_GROWTH_BYTES,
        )

    def test_release_workflow_consumes_current_prerequisite_schema(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        consumed_schemas = re.findall(
            r'browser\.get\("schema"\)\s*!=\s*"([^"]+)"',
            workflow,
        )
        self.assertEqual(
            consumed_schemas,
            [MODULE.PREREQUISITE_SCHEMA, MODULE.PREREQUISITE_SCHEMA],
        )
        self.assertEqual(
            workflow.count('"rxls.render-browser-behavior.v2"'),
            2,
        )
        self.assertEqual(workflow.count('browser.get("mode_proofs")'), 2)
        self.assertEqual(
            workflow.count('browser.get("pending_boundary_sha256")'),
            2,
        )
        self.assertEqual(
            workflow.count('proof.get("process_tree_baseline_bytes")'),
            2,
        )
        self.assertEqual(
            workflow.count('rss.get("minimum_growth_bytes")'),
            2,
        )
        self.assertEqual(workflow.count("minimum_growth != 100663296"), 2)
        self.assertEqual(
            workflow.count(
                'source_network["route_sha256"]\n'
                '                  == installed_network["route_sha256"]'
            ),
            2,
        )
        self.assertEqual(
            workflow.count(
                'source_network["csp_sha256"]\n'
                '                  == installed_network["csp_sha256"]'
            ),
            2,
        )

    def test_artifact_rejects_extra_members_and_digest_drift(self) -> None:
        summary_payload = MODULE._canonical_payload(self._summary())
        artifact = self.root / "artifact.zip"
        with zipfile.ZipFile(artifact, "w") as archive:
            archive.writestr(MODULE.SUMMARY_NAME, summary_payload)
            archive.writestr("raw.log", b"PASS leaked log\n")
        payload = artifact.read_bytes()
        arguments = {
            "artifact_id": 987,
            "artifact_name": f"render-browser-{HEAD_SHA}-{RUN_ID}-{RUN_ATTEMPT}",
            "artifact_size_bytes": len(payload),
            "artifact_digest": f"sha256:{hashlib.sha256(payload).hexdigest()}",
            "head_sha": HEAD_SHA,
            "platform": "linux",
            "repository": MODULE.EXPECTED_REPOSITORY,
            "workflow_run_id": RUN_ID,
            "workflow_run_attempt": RUN_ATTEMPT,
        }
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "artifact_file_set"):
            MODULE.validate_artifact(artifact, **arguments)
        arguments["artifact_digest"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "artifact_digest"):
            MODULE.validate_artifact(artifact, **arguments)

    def test_artifact_name_binds_sha_run_and_attempt(self) -> None:
        artifact = self.root / "artifact.zip"
        with zipfile.ZipFile(artifact, "w") as archive:
            archive.writestr(MODULE.SUMMARY_NAME, MODULE._canonical_payload(self._summary()))
        payload = artifact.read_bytes()
        with self.assertRaisesRegex(MODULE.BrowserEvidenceError, "artifact_name"):
            MODULE.validate_artifact(
                artifact,
                artifact_id=987,
                artifact_name="render-browser-wrong",
                artifact_size_bytes=len(payload),
                artifact_digest=f"sha256:{hashlib.sha256(payload).hexdigest()}",
                head_sha=HEAD_SHA,
                platform="linux",
                repository=MODULE.EXPECTED_REPOSITORY,
                workflow_run_id=RUN_ID,
                workflow_run_attempt=RUN_ATTEMPT,
            )


if __name__ == "__main__":
    unittest.main()
