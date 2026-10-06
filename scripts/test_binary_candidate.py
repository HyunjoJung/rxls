"""Archive tests use inert headers; no binary, Cargo, or native app is launched."""

import copy
import io
import json
from pathlib import Path
import stat
import struct
import subprocess
import tempfile
import unittest
from unittest import mock
import zipfile

import binary_candidate as candidate
import binary_process as processes
import check_workflow_policy as policy

REVISION = "a" * 40
LOCK_HASH = "b" * 64
ROOT_LOCK_HASH = "e" * 64


def binary(selected):
    data = bytearray(128)
    if selected == "windows-x64":
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 60, 64)
        data[64:68] = b"PE\0\0"
        struct.pack_into("<H", data, 68, 0x8664)
    elif selected == "linux-x64":
        data[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", data, 18, 62)
    else:
        data[:4] = b"\xcf\xfa\xed\xfe"
        struct.pack_into("<I", data, 4, 0x0100000C)
    return bytes(data)


def data_set(selected="windows-x64"):
    target = candidate.PLATFORMS[selected]["target"]
    payloads = {name: b"MIT test legal text\n" for name in candidate.LEGAL_FILES}
    payloads.update({name: binary(selected) for name in candidate.binary_paths(selected)})
    payloads["README.txt"] = b"Verification candidate\n"
    payloads["THIRD_PARTY_NOTICES.txt"] = f"RXLS MCP THIRD-PARTY NOTICES\n- Cargo lock SHA-256: {LOCK_HASH}\n".encode()
    payloads["CLI-THIRD-PARTY-NOTICES.txt"] = f"RXLS CLI THIRD-PARTY NOTICES\n- Cargo lock SHA-256: {ROOT_LOCK_HASH}\n".encode()
    manifest = {
        "schema": candidate.SCHEMA, "stage": "prepared",
        "source_revision": REVISION, "source_tree": "c" * 40,
        "source_archive_sha256": "d" * 64,
        "source_files": {name: ROOT_LOCK_HASH if name == "Cargo.lock" else LOCK_HASH for name in candidate.SOURCE_FILES},
        "versions": {"cli": "0.1.4", "mcp": "0.1.0"},
        "platform": selected, "target": target,
        "planned_runner": candidate.PLATFORMS[selected]["runner"],
        "notice_cli_generated": True, "host": {"system": candidate.PLATFORMS[selected]["system"]},
        "compilers": {name: {"toolchain": version, "rustc_verbose": f"rustc {version} (test)\nhost: {target}\n", "cargo_version": f"cargo {version} (test)"} for name, version in (("cli", "1.85.0"), ("mcp", "1.88.0"))},
        "files": {name: {"bytes": len(data), "sha256": candidate.digest(data), "mode": 0o755 if name in candidate.binary_paths(selected) else 0o644} for name, data in payloads.items()},
    }
    manifest["candidate"] = candidate.candidate_name("0.1.4", "0.1.0", REVISION, selected)
    return manifest, payloads


def write_archive(directory, manifest, payloads, *, extra=(), modes=None):
    stem = manifest["candidate"] if isinstance(manifest, dict) else candidate.candidate_name("0.1.4", "0.1.0", REVISION, "windows-x64")
    path = directory / (stem + ".zip")
    records = [(f"{stem}/{name}", data, 0o755 if name.startswith("bin/") else 0o644) for name, data in payloads.items()]
    records.append((f"{stem}/candidate.json", candidate.encoded(manifest), 0o644))
    records.extend(extra)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, allowZip64=False) as archive:
        for name, data, mode in records:
            info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (modes or {}).get(name, stat.S_IFREG | mode) << 16
            archive.writestr(info, data)
    return path


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def verify(self, path, *, revision=REVISION, selected="windows-x64", checksum=None):
        return candidate.read_archive(path, revision, selected, checksum or candidate.sha256_file(path))

    def test_three_native_envelopes_extract_exact_inventory_and_modes(self):
        for selected in candidate.PLATFORMS:
            with self.subTest(selected=selected):
                manifest, payloads = data_set(selected)
                path = write_archive(self.directory, manifest, payloads)
                checked, files = self.verify(path, selected=selected)
                self.assertEqual(files, payloads)
                destination = self.directory / selected
                candidate.extract_verified(destination, checked, files)
                self.assertEqual({str(item.relative_to(destination)).replace("\\", "/") for item in destination.rglob("*") if item.is_file()}, set(payloads))
                with self.assertRaisesRegex(ValueError, "fresh"):
                    candidate.extract_verified(destination, checked, files)

    def test_corruption_and_external_identity_are_rejected(self):
        manifest, payloads = data_set()
        path = write_archive(self.directory, manifest, payloads)
        for kwargs in ({"checksum": "0" * 64}, {"revision": "f" * 40}, {"selected": "linux-x64"}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                self.verify(path, **kwargs)
        original = path.read_bytes()
        path.write_bytes(original[:-1])
        with self.assertRaises(ValueError):
            self.verify(path)
        path.write_bytes(original + b"unbound trailing bytes")
        with self.assertRaises(ValueError):
            self.verify(path)

    def test_directory_count_rejected_before_zip_index_allocation(self):
        manifest, payloads = data_set()
        path = write_archive(self.directory, manifest, payloads)
        data = bytearray(path.read_bytes())
        struct.pack_into("<HH", data, len(data) - 22 + 8, 65535, 65535)
        path.write_bytes(data)
        with mock.patch.object(candidate.zipfile, "ZipFile", side_effect=AssertionError("must not allocate index")):
            with self.assertRaisesRegex(ValueError, "envelope/count"):
                self.verify(path)

    def test_manifest_shapes_versions_targets_and_compilers_are_rejected(self):
        original, payloads = data_set()
        changes = [lambda value: [], lambda value: value | {"versions": {"cli": "0.1.4", "mcp": True}}, lambda value: value | {"source_tree": "short"}, lambda value: value | {"source_files": {}}, lambda value: value | {"target": "aarch64-pc-windows-msvc"}, lambda value: value | {"planned_runner": "windows-latest"}, lambda value: value | {"notice_cli_generated": False}, lambda value: value | {"compilers": {}}]
        for change in changes:
            with self.subTest(change=changes.index(change)), self.assertRaises(ValueError):
                self.verify(write_archive(self.directory, change(copy.deepcopy(original)), payloads))
        manifest = copy.deepcopy(original)
        manifest["compilers"]["cli"]["rustc_verbose"] = "rustc 1.85.0 (test)\nhost: aarch64-apple-darwin\n"
        with self.assertRaises(ValueError):
            self.verify(write_archive(self.directory, manifest, payloads))

    def test_unexpected_missing_or_modified_inventory_is_rejected(self):
        manifest, payloads = data_set()
        for files in (payloads | {"extra.txt": b"extra"}, {name: data for name, data in payloads.items() if name != "LICENSE"}, payloads | {"LICENSE": b"modified"}):
            with self.subTest(files=set(files)), self.assertRaises(ValueError):
                self.verify(write_archive(self.directory, manifest, files))
        path = write_archive(self.directory, manifest, payloads)
        renamed = path.with_name("v0.1.4.zip")
        path.rename(renamed)
        with self.assertRaisesRegex(ValueError, "filename"):
            self.verify(renamed)

    def test_cli_notice_is_required_even_when_mcp_notice_remains_valid(self):
        manifest, payloads = data_set()
        payloads.pop("CLI-THIRD-PARTY-NOTICES.txt", None)
        manifest["files"].pop("CLI-THIRD-PARTY-NOTICES.txt", None)
        with self.assertRaisesRegex(ValueError, "inventory"):
            self.verify(write_archive(self.directory, manifest, payloads))

    def test_each_notice_is_bound_to_its_distinct_lock_and_title(self):
        original, payloads = data_set()
        self.verify(write_archive(self.directory, original, payloads))
        changes = (("CLI-THIRD-PARTY-NOTICES.txt", f"RXLS CLI THIRD-PARTY NOTICES\n- Cargo lock SHA-256: {LOCK_HASH}\n".encode()), ("THIRD_PARTY_NOTICES.txt", f"RXLS MCP THIRD-PARTY NOTICES\n- Cargo lock SHA-256: {ROOT_LOCK_HASH}\n".encode()), ("CLI-THIRD-PARTY-NOTICES.txt", f"RXLS MCP THIRD-PARTY NOTICES\n- Cargo lock SHA-256: {ROOT_LOCK_HASH}\n".encode()))
        for name, text in changes:
            with self.subTest(name=name, text=text):
                manifest = copy.deepcopy(original)
                files = payloads | {name: text}
                manifest["files"][name].update(bytes=len(text), sha256=candidate.digest(text))
                with self.assertRaisesRegex(ValueError, "notice/locked"):
                    self.verify(write_archive(self.directory, manifest, files))

    def test_unsafe_link_and_case_collision_are_rejected(self):
        manifest, payloads = data_set()
        stem = manifest["candidate"]
        paths = [f"{stem}/../escape", f"{stem}/bin/CON.exe", f"{stem}/bin/name:stream", f"{stem}/bin/name.", f"{stem}/bin\\escape", "/absolute", f"{stem}/license"]
        for name in paths:
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.verify(write_archive(self.directory, manifest, payloads, extra=[(name, b"x", 0o644)]))
        with self.assertRaisesRegex(ValueError, "regular"):
            self.verify(write_archive(self.directory, manifest, payloads, modes={f"{stem}/LICENSE": stat.S_IFLNK | 0o777}))

    def test_bound_modes_architecture_and_notice_are_rejected(self):
        original, payloads = data_set()
        stem = original["candidate"]
        with self.assertRaisesRegex(ValueError, "mode"):
            self.verify(write_archive(self.directory, original, payloads, modes={f"{stem}/bin/rxls.exe": stat.S_IFREG | 0o644}))
        for name, replacement in (("bin/rxls.exe", binary("linux-x64")), ("THIRD_PARTY_NOTICES.txt", b"wrong notice\n")):
            manifest = copy.deepcopy(original)
            files = payloads | {name: replacement}
            manifest["files"][name].update(bytes=len(replacement), sha256=candidate.digest(replacement))
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.verify(write_archive(self.directory, manifest, files))
        with mock.patch.object(candidate, "MAX_NOTICE", 1), self.assertRaises(ValueError):
            self.verify(write_archive(self.directory, original, payloads))

    def test_header_offsets_and_native_host_cannot_substitute_cross_compile(self):
        malformed = bytearray(binary("windows-x64"))
        struct.pack_into("<I", malformed, 60, 0xFFFFFFFF)
        with self.assertRaises(ValueError):
            candidate.verify_header(bytes(malformed), "windows-x64")
        manifest, _ = data_set()
        with mock.patch.object(candidate.platform, "system", return_value="Linux"), mock.patch.object(candidate.platform, "machine", return_value="x86_64"), self.assertRaisesRegex(ValueError, "native runner"):
            candidate.native_host("windows-x64", manifest["compilers"])


class WorkflowTests(unittest.TestCase):
    def test_pins_exact_pr_head_and_bounded_readonly_native_matrix(self):
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/binary-candidate.yml"
        text = workflow.read_text(encoding="utf-8")
        for audit in (policy.audit_action_pins, policy.audit_pr_head_checkouts, policy.audit_tool_commands):
            self.assertEqual(audit(workflow, text), [])
        for runner, target in (("windows-2022", "x86_64-pc-windows-msvc"), ("ubuntu-22.04", "x86_64-unknown-linux-gnu"), ("macos-15", "aarch64-apple-darwin")):
            self.assertIn(f"runner: {runner}", text)
            self.assertIn(f"target: {target}", text)
        self.assertEqual(text.count("contents: read"), 1)
        self.assertIn("timeout-minutes: 45", text)
        self.assertNotIn("contents: write", text)
        self.assertNotIn("cargo publish", text)
        self.assertNotIn("tags:", text)
        bad_pin = text.replace("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1", "actions/checkout@v7")
        self.assertTrue(policy.audit_action_pins(workflow, bad_pin))
        bad_revision = text.replace("ref: ${{ github.event.pull_request.head.sha || github.sha }}", "ref: main")
        self.assertTrue(policy.audit_pr_head_checkouts(workflow, bad_revision))


class OwnershipTests(unittest.TestCase):
    def test_constructor_cleanup_value_error_preserves_original_start_failure(self):
        events = []

        class Job:
            observations = []

            def assign(self, process):
                raise OSError("original start failure")

            def kill(self):
                events.append("job-kill")

            def close(self):
                events.append("job-close")
                raise ValueError("combined primary and close failure")

        child = mock.Mock()
        child.returncode = 1
        with mock.patch.object(processes, "WINDOWS", True), mock.patch.object(processes, "WindowsJob", Job), mock.patch.object(processes.subprocess, "Popen", return_value=child):
            with self.assertRaises(ValueError) as caught:
                processes.OwnedCommand(["inert-cargo"], Path.cwd())
        self.assertIn("original start failure", str(caught.exception))
        self.assertIn("combined primary and close failure", str(caught.exception))
        self.assertEqual(str(caught.exception.__cause__), "original start failure")
        child.kill.assert_called_once()
        child.wait.assert_called_once()
        self.assertEqual(events, ["job-kill", "job-close"])

    def test_constructor_failure_writes_bounded_start_and_exit_receipt(self):
        failure = ValueError("original startup failure: " + "x" * (64 * 1024))
        failure.owned_exit_code = 1
        failure.owned_observations = [{"phase": "after-terminate", "active_processes": 0}]
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(candidate, "OwnedCommand", side_effect=failure):
            log = Path(directory) / "start.log"
            with self.assertRaises(ValueError):
                candidate.run(["inert-cargo"], Path(directory), log)
            receipt = log.with_suffix(".exit.json")
            self.assertLess(receipt.stat().st_size, candidate.MAX_METADATA)
            record = json.loads(receipt.read_text())
            self.assertEqual(record["stage"], "start")
            self.assertEqual(record["status"], "failed")
            self.assertEqual(record["exit_code"], 1)
            self.assertEqual(record["output_bytes"], 0)
            self.assertIn("original startup failure", record["errors"][0])
            self.assertEqual(record["ownership_observations"], failure.owned_observations)
            self.assertEqual(log.read_bytes(), b"")

    def test_worker_gate_prevents_launch_and_preserves_command_exit(self):
        with mock.patch.object(processes.subprocess, "call", return_value=7) as launch:
            with self.assertRaisesRegex(ValueError, "gate"):
                processes.worker([json.dumps(["inert-cargo"])], io.BytesIO(b"wrong\n"))
            launch.assert_not_called()
            self.assertEqual(processes.worker([json.dumps(["inert-cargo", "+1.85.0"])], io.BytesIO(processes.GATE)), 7)
            launch.assert_called_once_with(["inert-cargo", "+1.85.0"], stdin=subprocess.DEVNULL, creationflags=0x08000000 if processes.os.name == "nt" else 0)

    def test_worker_hides_windows_command_and_keeps_posix_flags_zero(self):
        for platform, flags in (("nt", 0x08000000), ("posix", 0)):
            with self.subTest(platform=platform), mock.patch.object(processes.os, "name", platform), mock.patch.object(processes.subprocess, "call", return_value=9) as launch:
                self.assertEqual(processes.worker([json.dumps(["inert-cargo"])], io.BytesIO(processes.GATE)), 9)
                launch.assert_called_once_with(["inert-cargo"], stdin=subprocess.DEVNULL, creationflags=flags)

    def test_windows_assignment_precedes_gate_and_failure_cleans_only_owned_worker(self):
        for assignment_fails in (False, True):
            events = []

            class Sink:
                def write(self, data):
                    events.append("gate")
                    self_test.assertEqual(data, processes.GATE)

                def flush(self):
                    pass

                def close(self):
                    pass

            class Child:
                stdin = Sink()

                def kill(self):
                    events.append("worker-kill")

                def wait(self, timeout):
                    events.append("worker-wait")
                    return 1

                def poll(self):
                    return 0

            class Job:
                def assign(self, process):
                    events.append("assign")
                    if assignment_fails:
                        raise OSError("assignment failure")

                def kill(self):
                    events.append("job-kill")

                def close(self):
                    events.append("job-close")

            self_test = self
            with mock.patch.object(processes, "WINDOWS", True), mock.patch.object(processes, "WindowsJob", Job), mock.patch.object(processes.subprocess, "Popen", return_value=Child()):
                if assignment_fails:
                    with self.assertRaisesRegex(OSError, "assignment"):
                        processes.OwnedCommand(["inert-cargo"], Path.cwd())
                    self.assertNotIn("gate", events)
                    self.assertEqual(events, ["assign", "job-kill", "worker-kill", "worker-wait", "job-close"])
                else:
                    owner = processes.OwnedCommand(["inert-cargo"], Path.cwd())
                    owner.kill()
                    owner.close()
                    self.assertEqual(events, ["assign", "gate", "job-kill", "job-close"])

    def test_posix_termination_uses_only_owned_session(self):
        child = mock.Mock(pid=12345)
        child.poll.return_value = None
        with mock.patch.object(processes, "WINDOWS", False), mock.patch.object(processes.subprocess, "Popen", return_value=child) as launch, mock.patch.object(processes.os, "killpg", create=True) as kill, mock.patch.object(processes.signal, "SIGKILL", 9, create=True):
            owner = processes.OwnedCommand(["inert-cargo"], Path.cwd())
            self.assertTrue(launch.call_args.kwargs["start_new_session"])
            owner.kill()
            kill.assert_called_once_with(12345, processes.signal.SIGKILL)

    def test_timeout_records_actual_supervised_exit_and_always_closes_owner(self):
        class Child:
            stdout = io.BytesIO(b"owned diagnostic\n")
            killed = False

            def wait(self, timeout):
                if not self.killed:
                    raise subprocess.TimeoutExpired("inert-cargo", timeout)
                return -9

            def poll(self):
                return -9

        owner = mock.Mock(process=Child(), observations=[])
        owner.kill.side_effect = lambda: setattr(owner.process, "killed", True)
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(candidate, "OwnedCommand", return_value=owner):
            log = Path(directory) / "command.log"
            with self.assertRaisesRegex(ValueError, "failed"):
                candidate.run(["inert-cargo"], Path(directory), log, timeout=1)
            record = json.loads(log.with_suffix(".exit.json").read_text())
            self.assertEqual(record["exit_code"], -9)
            self.assertIn("command timeout", record["errors"])
            owner.kill.assert_called_once()
            owner.close.assert_called_once()

    def test_posix_exited_leader_still_cleans_owned_group(self):
        child = mock.Mock(pid=23456)
        child.poll.return_value = 0
        with mock.patch.object(processes, "WINDOWS", False), mock.patch.object(processes.subprocess, "Popen", return_value=child), mock.patch.object(processes.os, "killpg", create=True) as kill, mock.patch.object(processes.signal, "SIGKILL", 9, create=True):
            owner = processes.OwnedCommand(["inert-cargo"], Path.cwd())
            owner.close()
            kill.assert_called_once_with(23456, 9)
            child.wait.assert_not_called()


if __name__ == "__main__":
    unittest.main()
