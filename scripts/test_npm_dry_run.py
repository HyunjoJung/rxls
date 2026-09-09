#!/usr/bin/env python3
"""Exercise npm dry-run guards against a loopback-only existing-version registry.

Direct invocation requires POSIX, Node 24.18.0/npm 11.16.0 and fails if unavailable.
Ordinary unittest discovery skips only when this pinned runtime (including bash
and sha256sum) is unavailable; RXLS_REQUIRE_PINNED_NPM=1 makes discovery strict.
No credentials, real registry, package scripts, or dependencies are used.
"""

from __future__ import annotations

import base64
import gzip
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import threading
import time
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/render-package-release.yml"
PACKAGE = "rxls-npm-dry-run-fixture"
VERSION = "0.0.0"
MAX_OUTPUT_BYTES = 64 * 1024
COMMAND_TIMEOUT_SECONDS = 20
STRICT_ENV = "RXLS_REQUIRE_PINNED_NPM"


def isolated_environment(root: Path) -> dict[str, str]:
    home = root / "home"
    home.mkdir()
    user_config, global_config = root / "user.npmrc", root / "global.npmrc"
    user_config.write_text("", encoding="utf-8")
    global_config.write_text("", encoding="utf-8")
    return {
        "PATH": os.environ.get("PATH", os.defpath),
        "HOME": str(home),
        "TMPDIR": str(root),
        "LANG": "C",
        "CI": "1",
        "NO_COLOR": "1",
        "NPM_CONFIG_USERCONFIG": str(user_config),
        "NPM_CONFIG_GLOBALCONFIG": str(global_config),
        "NPM_CONFIG_CACHE": str(root / "npm-cache"),
        "NPM_CONFIG_REGISTRY": "http://127.0.0.1:1/",
        "NPM_CONFIG_UPDATE_NOTIFIER": "false",
        "NPM_CONFIG_AUDIT": "false",
        "NPM_CONFIG_FUND": "false",
        "NPM_CONFIG_FETCH_RETRIES": "0",
        "NPM_CONFIG_FETCH_TIMEOUT": "3000",
    }


def command(argv: list[str], root: Path, env: dict[str, str]) -> tuple[int, str]:
    """Bound captured bytes before growth, and terminate the whole command group."""
    process = subprocess.Popen(
        argv, cwd=root, env=env, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, start_new_session=True,
    )
    assert process.stdout is not None
    output = bytearray()
    deadline = time.monotonic() + COMMAND_TIMEOUT_SECONDS
    completed = False
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError("npm regression subprocess exceeded 20 seconds")
                if not selector.select(min(remaining, 0.25)):
                    continue
                chunk = os.read(process.stdout.fileno(), min(4096, MAX_OUTPUT_BYTES - len(output) + 1))
                if not chunk:
                    break
                if len(output) + len(chunk) > MAX_OUTPUT_BYTES:
                    raise AssertionError("npm regression subprocess exceeded 64 KiB output")
                output.extend(chunk)
        status = process.wait(timeout=max(0.01, deadline - time.monotonic()))
        completed = True
        return status, output.decode("utf-8", errors="replace")
    finally:
        if not completed or process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.wait(timeout=5)
        process.stdout.close()


def workflow_fragment() -> str:
    """Use actual publish/event/checksum statements, excluding only browser proof."""
    workflow = WORKFLOW.read_text(encoding="utf-8")
    step = workflow.split("      - name: Pack, inspect, dry-run, and consume\n", 1)[1]
    body = textwrap.dedent(step.split("        run: |\n", 1)[1].split("      - name:", 1)[0])
    fragment = body.split('  --write-report "$output/package-report.json"\n', 1)[1]
    fragment = fragment.split('consumer="$RUNNER_TEMP/render-worker-consumer"\n', 1)[0]
    fragment, removed = re.subn(
        r"(?ms)^  ARCHIVE=\"\$archive\" python3 - <<'PY'\n.*?^PY\n",
        "  : # Browser prerequisite proof is independently covered.\n",
        fragment,
    )
    if removed != 1 or "${{" in fragment:
        raise AssertionError("unexpected workflow dry-run fragment boundary")
    return 'set -euo pipefail\narchive="$PWD/fixture.tgz"\noutput="$PWD/output"\n' + fragment


class NpmDryRunTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        try:
            if os.name != "posix":
                raise RuntimeError("POSIX process groups and pipe selectors are required")
            for executable in ("node", "npm", "bash", "sha256sum"):
                if shutil.which(executable) is None:
                    raise RuntimeError(f"missing {executable}")
            with tempfile.TemporaryDirectory(prefix="rxls-npm-runtime-") as temporary:
                root = Path(temporary)
                env = isolated_environment(root)
                for executable, expected in (("node", "v24.18.0"), ("npm", "11.16.0")):
                    status, output = command([executable, "--version"], root, env)
                    if status != 0 or output.strip() != expected:
                        raise RuntimeError(f"expected {executable} {expected}, got {output.strip()[:160]!r}")
        except (OSError, RuntimeError, AssertionError, subprocess.SubprocessError) as error:
            message = f"Pinned npm dry-run runtime unavailable: {error}"
            if os.environ.get(STRICT_ENV) == "1":
                raise RuntimeError(message) from error
            raise unittest.SkipTest(message + f"; set {STRICT_ENV}=1 to require it") from error

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="rxls-npm-dry-run-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.env = isolated_environment(self.root)
        (self.root / "output").mkdir()
        metadata = {"name": PACKAGE, "version": VERSION, "license": "MIT"}
        package_json = json.dumps(metadata).encode("utf-8")
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            member = tarfile.TarInfo("package/package.json")
            member.size, member.mode, member.mtime = len(package_json), 0o644, 0
            tar.addfile(member, io.BytesIO(package_json))
        self.original = gzip.compress(archive.getvalue(), mtime=0)
        self.archive = self.root / "fixture.tgz"
        self.archive.write_bytes(self.original)
        self.requests: list[tuple[str, str, bool]] = []
        requests = self.requests
        integrity = "sha512-" + base64.b64encode(hashlib.sha512(self.original).digest()).decode("ascii")

        class Registry(BaseHTTPRequestHandler):
            def log_message(self, *_args: object) -> None:
                pass

            def respond(self) -> None:
                requests.append((self.command, self.path, "Authorization" in self.headers))
                status = 200 if self.command in {"GET", "HEAD"} and self.path == "/" + PACKAGE else 405
                if len(requests) > 32:
                    status = 429
                payload = json.dumps({
                    "name": PACKAGE, "dist-tags": {"latest": VERSION},
                    "versions": {VERSION: {**metadata, "dist": {
                        "integrity": integrity,
                        "tarball": f"http://127.0.0.1:{self.server.server_port}/fixture.tgz",
                    }}},
                }).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                if self.command != "HEAD":
                    self.wfile.write(payload)

            do_GET = do_HEAD = do_PUT = do_POST = do_DELETE = do_PATCH = do_OPTIONS = respond

        server = ThreadingHTTPServer(("127.0.0.1", 0), Registry)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 2)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.env["NPM_CONFIG_REGISTRY"] = f"http://127.0.0.1:{server.server_port}/"
        self.addCleanup(self.assert_safety)

    def assert_safety(self) -> None:
        self.assertEqual(self.archive.read_bytes(), self.original, "dry run changed the TGZ")
        self.assertTrue(all(method in {"GET", "HEAD"} for method, _, _ in self.requests), self.requests)
        self.assertTrue(all(not auth for _, _, auth in self.requests), "registry received credentials")
        self.assertLessEqual(len(self.requests), 32)

    def run_workflow(self, event: str) -> tuple[int, str]:
        return command(["bash", "--noprofile", "--norc", "-c", workflow_fragment()], self.root, {**self.env, "GITHUB_EVENT_NAME": event})

    def assert_existing_version_rejected(self, result: tuple[int, str]) -> None:
        status, output = result
        self.assertNotEqual(status, 0, output[-4000:])
        self.assertRegex(output, r"(?i)(previously published|already exists|cannot publish over|EPUBLISHCONFLICT)")
        self.assertTrue(self.requests, "npm did not query the existing-version registry")

    def test_manual_rehearsal_accepts_unchanged_existing_version_without_writes(self) -> None:
        self.assert_existing_version_rejected(command(
            ["npm", "publish", "--dry-run", "--ignore-scripts", "--access", "public", str(self.archive)],
            self.root, self.env,
        ))
        status, output = self.run_workflow("workflow_dispatch")
        self.assertEqual(status, 0, output[-4000:])
        self.assertIn("workflow_dispatch packaging rehearsal; registry availability not checked", output)
        self.assertIn("fixture.tgz: OK", output)
        self.assertTrue((self.root / "output/npm-publish-dry-run.txt").is_file())

    def test_tag_dry_run_still_rejects_existing_version(self) -> None:
        self.assert_existing_version_rejected(self.run_workflow("push"))

    def test_unsupported_event_stops_before_registry_access(self) -> None:
        status, output = self.run_workflow("pull_request")
        self.assertNotEqual(status, 0, output[-4000:])
        self.assertEqual(self.requests, [])

    def test_inherited_force_stops_before_registry_access(self) -> None:
        config = Path(self.env["NPM_CONFIG_GLOBALCONFIG"])
        for source in ("global_config", "environment"):
            with self.subTest(source=source):
                config.write_text("force=true\n" if source == "global_config" else "", encoding="utf-8")
                if source == "environment":
                    self.env["NPM_CONFIG_FORCE"] = "true"
                status, output = self.run_workflow("workflow_dispatch")
                self.assertNotEqual(status, 0, output[-4000:])
                self.assertEqual(self.requests, [])

    def test_protected_publish_prefix_rejects_inherited_force(self) -> None:
        step = WORKFLOW.read_text(encoding="utf-8").split(
            "      - name: Publish exact package with provenance\n", 1,
        )[1]
        body = textwrap.dedent(step.split("        run: |\n", 1)[1].split("      - name:", 1)[0])
        # Never execute the version expansion or actual publish, even if the
        # guard is removed: only this prefix can reach the subprocess.
        prefix, separator, _ = body.partition('version="${{ steps.package.outputs.version }}"')
        self.assertTrue(separator, "protected publish prefix boundary changed")
        self.assertNotIn("publish", prefix)
        self.assertNotIn("${{", prefix)
        Path(self.env["NPM_CONFIG_GLOBALCONFIG"]).write_text("force=true\n", encoding="utf-8")
        status, output = command(
            ["bash", "--noprofile", "--norc", "-c", "set -euo pipefail\n" + prefix],
            self.root, self.env,
        )
        self.assertNotEqual(status, 0, output[-4000:])
        self.assertEqual(self.requests, [])


@unittest.skipUnless(os.name == "posix", "bounded command fixture requires POSIX process groups")
class NpmHarnessBoundTests(unittest.TestCase):
    def test_subprocess_output_overflow_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="rxls-npm-bound-") as temporary:
            root = Path(temporary)
            with self.assertRaisesRegex(AssertionError, "64 KiB output"):
                command([sys.executable, "-c", "print('x' * 65537)"], root, isolated_environment(root))

    def test_subprocess_deadline_is_enforced(self) -> None:
        with tempfile.TemporaryDirectory(prefix="rxls-npm-bound-") as temporary:
            root = Path(temporary)
            with patch.dict(command.__globals__, COMMAND_TIMEOUT_SECONDS=0.05):
                with self.assertRaisesRegex(AssertionError, "exceeded"):
                    command([sys.executable, "-c", "import time; time.sleep(5)"], root, isolated_environment(root))

    def test_deadline_kills_descendant_after_process_leader_exits(self) -> None:
        with tempfile.TemporaryDirectory(prefix="rxls-npm-bound-") as temporary:
            root = Path(temporary)
            marker = root / "descendant-survived"
            script = (
                "import os,pathlib,sys,time\n"
                "if os.fork(): sys.exit(0)\n"
                "time.sleep(0.3)\n"
                "pathlib.Path(sys.argv[1]).write_text('survived')\n"
            )
            with patch.dict(command.__globals__, COMMAND_TIMEOUT_SECONDS=0.1):
                with self.assertRaisesRegex(AssertionError, "exceeded"):
                    command([sys.executable, "-c", script, str(marker)], root, isolated_environment(root))
            time.sleep(0.4)
            self.assertFalse(marker.exists(), "timed-out descendant outlived its exited process leader")


if __name__ == "__main__":
    os.environ[STRICT_ENV] = "1"
    unittest.main()
