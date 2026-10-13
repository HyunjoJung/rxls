"""Pure protocol/cache regressions; inert unit tests never launch subprocesses."""

import io
import json
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest import mock
import zipfile
import xml.etree.ElementTree as ET

import binary_smoke as smoke


def wire(value):
    return (json.dumps(value) + "\n").encode()


class ProtocolTests(unittest.TestCase):
    def test_response_identity_and_errors_are_not_accepted_as_success(self):
        good = {"jsonrpc": "2.0", "id": 1, "result": {"tools": []}}
        self.assertEqual(smoke.decode_response(wire(good), 1), good["result"])
        for value in (good | {"id": 2}, good | {"id": True}, good | {"jsonrpc": "1.0"}, good | {"error": {"code": -32603}}, good | {"result": None}, {"jsonrpc": "2.0", "method": "notifications/progress"}, []):
            with self.subTest(value=value), self.assertRaises(ValueError):
                smoke.decode_response(wire(value), 1)
        for value in (b"invalid\n", wire(good).rstrip(), b"x" * (smoke.MAX_LINE + 1) + b"\n"):
            with self.assertRaises(ValueError):
                smoke.decode_response(value, 1)

    def test_tool_error_and_result_shape_are_not_accepted(self):
        self.assertEqual(smoke.tool_content({"structuredContent": {"ok": True}}), {"ok": True})
        for value in ({"isError": True, "structuredContent": {}}, {}, {"structuredContent": []}):
            with self.assertRaises(ValueError):
                smoke.tool_content(value)

    def test_recalculation_counts_and_saved_cache_are_exact(self):
        summary = {"applied_edits": 1, "recalculation": {"computed_cells": 1, "unchanged_cells": 0, "unsupported_cells": 0, "reasons": []}}
        smoke.expect_summary(summary)
        for value in (summary | {"applied_edits": True}, summary | {"recalculation": summary["recalculation"] | {"computed_cells": 0}}, summary | {"recalculation": summary["recalculation"] | {"unsupported_cells": 1}}, summary | {"recalculation": None}):
            with self.assertRaises(ValueError):
                smoke.expect_summary(value)
        read = {"rows": [[{"value": {"type": "number", "value": 9}}, {"value": {"type": "formula", "formula": "A1*2", "cached": {"type": "number", "value": 18}}}]]}
        smoke.expect_read(read, 9)
        for value in ({}, {"rows": []}, {"rows": [[{"value": {"type": "number", "value": 9}}, {"value": {"type": "formula", "formula": "A1*2", "cached": {"type": "number", "value": 14}}}]]}, {"rows": [[{"value": {"type": "number", "value": 9}}, {"value": {"type": "number", "value": 18}}]]}):
            with self.assertRaises(ValueError):
                smoke.expect_read(value, 9)

    def test_transcript_is_bounded_before_append(self):
        client = smoke.Client.__new__(smoke.Client)
        client.transcript, client.transcript_bytes = [], 0
        client.record("response", {"result": {}})
        before = list(client.transcript)
        with mock.patch.object(smoke, "MAX_TRANSCRIPT", 32), self.assertRaises(ValueError):
            client.record("response", {"result": "x" * 64})
        self.assertEqual(client.transcript, before)
        client.transcript = [{}] * 64
        with self.assertRaises(ValueError):
            client.record("response", {})

    def test_version_command_caps_output_during_read_and_owns_cleanup(self):
        class OwnedProcess:
            def __init__(self, output, *, timeout=False):
                self.stdout, self.stderr = io.BytesIO(output), io.BytesIO()
                self.killed = False
                self.timeout = timeout

            def kill(self):
                self.killed = True

            def wait(self, timeout):
                if self.timeout and not self.killed:
                    raise subprocess.TimeoutExpired("inert", timeout)
                return -9 if self.killed else 0

        for output, timeout in ((b"x" * (smoke.MAX_STDERR + 1), False), (b"", True)):
            owned = OwnedProcess(output, timeout=timeout)
            with tempfile.TemporaryDirectory() as directory, mock.patch.object(smoke.subprocess, "Popen", return_value=owned), self.assertRaises(ValueError):
                smoke.command(Path("inert-binary"), ["--version"], Path(directory))
            self.assertTrue(owned.killed)

    def test_mcp_deadline_unblocks_input_and_cleanup_survives_broken_pipe(self):
        class OwnedProcess:
            def __init__(self):
                self.stdin = self
                self.stdout, self.stderr = io.BytesIO(), io.BytesIO()
                self.killed = threading.Event()
                self.waited = False

            def poll(self):
                return -9 if self.killed.is_set() else None

            def kill(self):
                self.killed.set()

            def write(self, data):
                if not self.killed.wait(timeout=1):
                    raise AssertionError("watchdog did not unblock owned input")
                raise BrokenPipeError("owned mock stopped")

            def flush(self):
                raise AssertionError("broken write must not flush")

            def close(self):
                raise BrokenPipeError("owned mock close")

            def wait(self, timeout):
                self.waited = True
                return -9

        owned = OwnedProcess()
        with mock.patch.object(smoke.subprocess, "Popen", return_value=owned):
            client = smoke.Client(Path("inert-mcp"), Path("owned"), time.monotonic() + 0.02)
            with self.assertRaisesRegex(ValueError, "write/flush"):
                client.send({"jsonrpc": "2.0", "id": 1})
            with self.assertRaisesRegex(ValueError, "exit/stderr/reader"):
                client.close()
        self.assertTrue(owned.killed.is_set())
        self.assertTrue(owned.waited)
        self.assertTrue(all(not thread.is_alive() for thread in client.threads))
        self.assertTrue(any("input close" in error for error in client.errors))

    def test_primary_stage_and_exit_survive_secondary_close_failure(self):
        class InertClient:
            transcript = []
            stderr = b"owned diagnostic"
            process = SimpleNamespace(returncode=5)

            def __init__(self, *args):
                pass

            def call(self, *args):
                raise ValueError("primary handshake failure")

            def close(self):
                raise ValueError("secondary close failure")

        def command(binary, arguments, runtime):
            if arguments == ["--version"]:
                return b"rxls-mcp 0.1.0\n" if "mcp" in binary.name else b"rxls 0.1.4\n"
            if arguments == ["--help"]:
                return b"rxls-mcp 0.1.0\n\nUSAGE:\n" if "mcp" in binary.name else b"usage: rxls\n"
            return b"7,14\n"

        with tempfile.TemporaryDirectory() as directory, mock.patch.object(smoke, "Client", InertClient), mock.patch.object(smoke, "command", side_effect=command):
            runtime = Path(directory) / "runtime"
            with self.assertRaisesRegex(ValueError, "primary handshake"):
                smoke.smoke(Path(directory), {"platform": "windows-x64", "versions": {"cli": "0.1.4", "mcp": "0.1.0"}}, runtime)
            result = json.loads((runtime / "stdio-result.json").read_text())
            self.assertEqual(result["status"], "failed")
            self.assertEqual(result["last_stage"], "initialize")
            self.assertEqual(result["exit_code"], 5)
            self.assertEqual(result["primary_failure"], "primary handshake failure")
            self.assertEqual(result["cleanup_failure"], "secondary close failure")

    def test_fixture_formula_and_package_are_small_and_standard(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "input.xlsx"
            data = smoke.make_fixture(path)
            self.assertLess(len(data), 16 * 1024)
            with zipfile.ZipFile(path) as archive:
                self.assertEqual(len(archive.infolist()), 6)
                xml = ET.fromstring(archive.read("xl/worksheets/sheet1.xml"))
                self.assertEqual(xml.findtext(".//s:c[@r='B1']/s:f", "", smoke.NS), "A1*2")
                self.assertEqual(xml.findtext(".//s:c[@r='B1']/s:v", "", smoke.NS), "14")
                workbook = ET.fromstring(archive.read("xl/workbook.xml"))
                self.assertEqual(workbook.find("s:calcPr", smoke.NS).get("calcMode"), "manual")


if __name__ == "__main__":
    unittest.main()
