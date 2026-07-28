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
        self.archive = self.root / "rxls-render-worker-0.1.2.tgz"
        self.archive.write_bytes(b"deterministic npm archive fixture\n" * 64)
        archive = self.archive.read_bytes()
        integrity = base64.b64encode(hashlib.sha512(archive).digest()).decode("ascii")
        self.pack = self.root / "npm-pack.json"
        self.pack.write_text(
            json.dumps(
                [
                    {
                        "name": "@rxls/render-worker",
                        "version": "0.1.2",
                        "filename": self.archive.name,
                        "size": len(archive),
                        "unpackedSize": 12,
                        "shasum": hashlib.sha1(archive).hexdigest(),
                        "integrity": f"sha512-{integrity}",
                        "entryCount": 12,
                        "files": [
                            {"path": path, "size": 1}
                            for path in (
                                "LICENSE",
                                "README.md",
                                "THIRD_PARTY_NOTICES.txt",
                                "js/client.mjs",
                                "js/protocol.mjs",
                                "js/worker-runtime.mjs",
                                "js/worker.mjs",
                                "package.json",
                                "pkg/rxls_render_wasm.js",
                                "pkg/rxls_render_wasm_bg.wasm",
                                "pkg/rxls_render_wasm_bg.wasm.d.ts",
                                "pkg/rxls_render_wasm.d.ts",
                            )
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
    ) -> str:
        growth = max(0, retained - baseline)
        rss_peak_growth = max(0, rss_peak - rss_baseline)
        rss_retained_growth = max(0, rss_retained - rss_baseline)
        if wasm_url is None:
            path = MODULE.EXPECTED_WASM_PATHS[mode]
            wasm_url = f"http://127.0.0.1:43210{path}"
        return (
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
        self.assertEqual(summary["package"]["entry_count"], 12)
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
