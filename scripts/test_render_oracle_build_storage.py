#!/usr/bin/env python3
"""Rootless command-stub tests for the hosted oracle storage lifecycle."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("render-oracle-build-storage.sh")
BASH = shutil.which("bash") or "/bin/bash"
# A transaction launches many Python command stubs; leave headroom under full
# discovery while retaining a finite test-only deadline and cleanup budget.
HELPER_TIMEOUT_SECONDS = 60
HELPER_CLEANUP_TIMEOUT_SECONDS = 5
SEED = "00000000-0000-0000-0000-000000000014"
UUID = "27e2ccf5-fd0a-452d-bb3f-69d80d5bd70d"
CACHE_NAMES = [
    "4c599c202bc5c08e2d34565a40eac3b2-le64.cache-8",
    "7ef2298fde41cc6eeb7af42e48b7d293-le64.cache-8",
    "d589a48862398ed80a3d6066f4f56f4c-le64.cache-8",
    "3830d5c3ddfd5cd38a049b759396e72e-le64.cache-8",
]

# Every privileged/GNU-specific program is replaced by this stateful dispatcher.
# No fake can fall through to the real Docker, filesystem, or service command.
STUB = r'''
import hashlib, json, os, pathlib, shutil, signal, stat, sys, tempfile
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
root = pathlib.Path(os.environ["FAKE_ROOT"])
state_path = root / "fake-state.json"
state = json.loads(state_path.read_text())
trace = root / "trace.jsonl"
with trace.open("a") as stream:
    stream.write(json.dumps([name, *args]) + "\n")
def save():
    state_path.write_text(json.dumps(state))
def done(output="", code=0):
    save()
    if output:
        sys.stdout.write(output)
    sys.exit(code)
def fail_event(event):
    if state.get("signal_on") == event:
        state["signal_on"] = None
        save()
        os.kill(os.getppid(), signal.SIGTERM)
        sys.exit(143)
    failures = state.setdefault("failures", {})
    if failures.get(event, 0):
        failures[event] -= 1
        done("", 42)
def safe(value):
    path = pathlib.Path(value)
    if not path.is_absolute():
        path = pathlib.Path.cwd() / path
    if not path.resolve().is_relative_to(root.resolve()):
        sys.stderr.write("stub_refused_path:" + str(path) + "\n")
        done("", 93)
    return path
volumes = state["volumes_path"]
if name == "timeout":
    while args and args[0].startswith("-"):
        args.pop(0)
    if args:
        args.pop(0)
    if not args or "/" in args[0]:
        done("", 94)
    target = root / "bin" / args[0]
    if not target.is_file():
        done("", 94)
    os.execv(str(target), [str(target), *args[1:]])
elif name == "id":
    done(str(state.get("uid", 0)) + "\n")
elif name == "flock":
    fail_event("flock")
    if args != ["-w", "5", "9"]:
        done("", 96)
    done("", 1 if state.get("lock_busy") else 0)
elif name == "uname":
    done((state.get("machine", "x86_64") if "-m" in args else "Linux") + "\n")
elif name == "lsb_release":
    done((state.get("distribution", "Ubuntu") if "-is" in args else state.get("release", "24.04")) + "\n")
elif name == "stat":
    path = safe(args[-1])
    if not path.exists() and not path.is_symlink():
        done("", 1)
    fmt = args[args.index("-c") + 1] if "-c" in args else args[args.index("--format") + 1] if "--format" in args else next((a[2:] for a in args if a.startswith("-c") and len(a)>2), "%u")
    mode = path.lstat().st_mode
    owner = "1001" if str(path) == state["runner_temp"] else "0"
    size = state.get("image_sizes", {}).get(str(path), path.lstat().st_size)
    values = {"%u": owner, "%g": owner, "%a": oct(stat.S_IMODE(mode))[2:], "%s": str(size), "%F": "directory" if path.is_dir() else "regular file", "%h": "1", "%d": "1", "%i": "123"}
    for key, value in values.items():
        fmt = fmt.replace(key, value)
    done(fmt + "\n")
elif name == "docker":
    if args[:1] == ["info"]:
        fmt = args[-1]
        values = {"{{.DockerRootDir}}": state["docker_root"], "{{.Architecture}}": "x86_64", "{{.Driver}}": "overlay2", "{{.LiveRestoreEnabled}}": "false"}
        for key, value in values.items():
            fmt = fmt.replace(key, value)
        done(fmt + "\n")
    if args[:2] == ["context", "show"]:
        done("default\n")
    if args[:2] == ["context", "inspect"]:
        if "--format" in args:
            done("unix:///var/run/docker.sock\n")
        done(json.dumps([{"Name":"default", "Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]) + "\n")
    if args[:1] == ["ps"]:
        fail_event("container_inventory")
        done("".join(x + "\n" for x in state["containers"]))
    if args[:2] == ["volume", "ls"]:
        fail_event("volume_inventory")
        done("".join(x + "\n" for x in state["volumes"]))
elif name == "systemctl":
    action = args[0]
    if action == "show":
        fail_event("daemon_inventory")
        unit = args[-1]
        done(("active" if state.get("socket_active" if unit.endswith(".socket") else "daemon_active") else "inactive") + "\n")
    if action == "is-active":
        unit = args[-1]
        done("", 0 if state.get("socket_active" if unit.endswith(".socket") else "daemon_active") else 3)
    if action in {"start", "stop"}:
        fail_event("systemctl_" + action)
        for unit in args[1:]:
            state["socket_active" if unit.endswith(".socket") else "daemon_active"] = action == "start"
        if action == "start" and state.get("mutate_original_metadata_on_start") and volumes not in state["mounts"]:
            metadata = safe(volumes) / "metadata.db"
            metadata.write_bytes(metadata.read_bytes() + b"\nlegitimate-docker-startup")
        done()
elif name == "ctr":
    fail_event("task_inventory")
    done("".join(x + "\n" for x in state["tasks"]))
elif name == "df":
    done("Avail\n999999999999\n")
elif name == "truncate":
    fail_event("truncate")
    path = safe(args[-1])
    if state.get("failures", {}).get("truncate_partial", 0):
        path.write_bytes(b"")
        state.setdefault("image_sizes", {})[str(path)] = 0
        fail_event("truncate_partial")
    if not path.exists():
        path.write_bytes(b"fixture-image")
    state.setdefault("image_sizes", {})[str(path)] = int(args[args.index("-s") + 1])
    done()
elif name == "mkfs.ext4":
    fail_event("mkfs.ext4")
    safe(args[-1])
    state["formatted"] = True
    state["uuid"] = args[args.index("-U") + 1] if "-U" in args else state["uuid"]
    extended = dict(item.split("=", 1) for item in args[args.index("-E") + 1].split(",")) if "-E" in args else {}
    state["seed"] = extended.get("hash_seed", "missing-seed")
    done()
elif name == "mount":
    source, target = args[-2:]
    safe(target)
    if "--bind" in args:
        fail_event("mount_bind")
        safe(source)
        state["mounts"][target] = {"SOURCE":"/dev/loop7", "FSTYPE":"ext4", "FSROOT":"/volumes", "MAJ:MIN":"7:7", "TARGET":target}
    else:
        fail_event("mount_loop")
        if source.startswith("/dev/loop"):
            if not state["loop_image"]:
                done("", 95)
        else:
            safe(source)
            state["loop_image"] = source
        state["mounts"][target] = {"SOURCE":"/dev/loop7", "FSTYPE":"ext4", "FSROOT":"/", "MAJ:MIN":"7:7", "TARGET":target}
    done()
elif name == "mountpoint":
    done("", 0 if args[-1] in state["mounts"] else 1)
elif name == "findmnt":
    option = "-o" if "-o" in args else "--output"
    columns = args[args.index(option) + 1].split(",") if option in args else ["SOURCE"]
    selector = next((x for x in ("-M", "--mountpoint", "-T", "--target", "-R", "--submounts") if x in args), None)
    target = args[args.index(selector) + 1] if selector else args[-1]
    entry = state["mounts"].get(target)
    if selector in {"-M", "--mountpoint", "-R", "--submounts"} and entry is None:
        done("", 1)
    if entry is None:
        entry = {"SOURCE":"/dev/root", "FSTYPE":"ext4", "FSROOT":"/", "MAJ:MIN":"8:1", "TARGET":"/"}
    entries = [entry]
    if selector in {"-R", "--submounts"}:
        entries.extend(value for key, value in state["mounts"].items() if key.startswith(target.rstrip("/") + "/"))
    done("".join(" ".join(row.get(column, "") for column in columns) + "\n" for row in entries))
elif name == "losetup":
    if "--associated" in args or "-j" in args:
        fail_event("loop_inventory")
        done("/dev/loop7\n" if state["loop_image"] else "")
    if "--detach" in args or "-d" in args:
        fail_event("loop_detach")
        if state["mounts"]:
            done("", 1)
        state["loop_image"] = None
        done()
    if "--find" in args or "-f" in args:
        fail_event("loop_attach")
        state["loop_image"] = str(safe(args[-1]))
        done("/dev/loop7\n")
    done((state.get("foreign_backing") or state["loop_image"] or "") + "\n")
elif name == "blkid":
    done(state["uuid"] + "\n")
elif name == "tune2fs":
    done("Directory Hash Seed:      " + state["seed"] + "\nDefault directory hash:   half_md4\nFilesystem features:      dir_index filetype extent\n")
elif name == "umount":
    target = args[-1]
    safe(target)
    fail_event("umount_bind" if target == volumes else "umount_loop")
    if target not in state["mounts"]:
        done("", 1)
    del state["mounts"][target]
    if target != volumes:
        # Unmount exposes the previously empty mountpoint, not the fake ext4
        # directory entries. An explicitly allocated loop remains attached.
        for child in safe(target).iterdir():
            if child.is_dir() and not child.is_symlink():
                shutil.rmtree(child)
            else:
                child.unlink()
    done()
elif name == "mktemp":
    template = safe(args[-1])
    prefix = template.name.rstrip("X")
    done(tempfile.mkdtemp(prefix=prefix, dir=template.parent) + "\n")
elif name == "ls":
    path = safe(args[-1])
    if "-1U" in args or "-U" in args:
        fail_event("order_probe")
        names = state["cache_names"]
        if sorted(entry.name for entry in path.iterdir()) != sorted(names):
            done("", 97)
        if state.get("wrong_order"):
            names = list(reversed(names))
        done("".join(x + "\n" for x in names))
    done("".join(x.name + "\n" for x in path.iterdir()))
elif name == "sha256sum":
    if "--check" in args or "-c" in args:
        rows = safe(args[-1]).read_text().splitlines()
        for row in rows:
            digest, filename = row.split(None, 1)
            filename = filename.lstrip(" *")
            if hashlib.sha256(safe(filename).read_bytes()).hexdigest() != digest:
                done("", 1)
        done()
    path = safe(args[-1])
    done(hashlib.sha256(path.read_bytes()).hexdigest() + "  " + str(path) + "\n")
elif name in {"mkdir", "install", "touch", "chmod", "chown", "rm", "rmdir"}:
    operands = []
    skip = False
    for arg in args:
        if skip:
            skip = False
            continue
        if arg in {"-m", "-o", "-g"}:
            skip = True
            continue
        if arg.startswith("-"):
            continue
        operands.append(arg)
    if name in {"chmod", "chown"}:
        operands = operands[1:]
    for value in operands:
        path = safe(value)
        if name in {"mkdir", "install"}:
            mode = int(args[args.index("-m") + 1], 8) if "-m" in args else 0o755
            path.mkdir(mode=mode, parents=True, exist_ok=True)
        elif name == "touch":
            path.touch()
        elif name == "chmod":
            path.chmod(int(args[0], 8))
        elif name == "chown":
            pass
        elif name == "rm":
            path.unlink(missing_ok=True)
        else:
            path.rmdir()
    done()
sys.stderr.write("stub_unsupported:" + json.dumps([name, *args]) + "\n")
done("", 96)
'''


class StorageHarness:
    def __init__(self, root: Path):
        self.root = root.resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.runner_temp = self.root / "runner-temp"
        self.runner_temp.mkdir()
        self.docker_root = self.root / "docker"
        (self.docker_root / "volumes").mkdir(parents=True)
        (self.docker_root / "volumes" / "metadata.db").write_bytes(b"original-metadata")
        self.job_state = self.runner_temp / "rxls-oracle-build-storage-123-1-oracle"
        self.state_path = self.root / "fake-state.json"
        self.write_state({
            "runner_temp": str(self.runner_temp), "docker_root": str(self.docker_root),
            "volumes_path": str(self.docker_root / "volumes"), "uid": 0,
            "daemon_active": True, "socket_active": True,
            "containers": [], "volumes": [], "tasks": [], "mounts": {},
            "loop_image": None, "uuid": UUID, "seed": SEED, "cache_names": CACHE_NAMES,
        })
        # Keep the interpreter path out of the shebang: virtual environments may
        # live beneath paths with spaces. PATH remains fixture-only and resolves
        # python3 to the exact interpreter running this test suite.
        (self.bin / "python3").symlink_to(sys.executable)
        dispatcher = self.bin / "dispatcher"
        dispatcher.write_text("#!/usr/bin/env python3\n" + STUB, encoding="utf-8")
        dispatcher.chmod(0o755)
        for command in (
            "timeout", "id", "flock", "uname", "lsb_release", "stat", "docker", "systemctl", "ctr",
            "df", "truncate", "mkfs.ext4", "mount", "mountpoint", "findmnt", "losetup",
            "blkid", "tune2fs", "umount", "ls", "sha256sum", "mkdir", "install", "touch",
            "chmod", "chown", "rm", "rmdir", "mktemp",
        ):
            (self.bin / command).symlink_to(dispatcher)
        # Read-only filters and fixture-local traversal need no privileged shim.
        for command in ("awk", "grep", "sed", "wc", "head", "tail", "cat", "find", "basename", "dirname", "sort", "cut", "tr", "date", "readlink", "realpath", "sync", "env"):
            actual = shutil.which(command)
            if actual:
                (self.bin / command).symlink_to(actual)
        self.env = {
            "PATH": str(self.bin), "HOME": str(self.root), "FAKE_ROOT": str(self.root),
            "RUNNER_TEMP": str(self.runner_temp), "GITHUB_ACTIONS": "true",
            "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
            "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_JOB": "oracle",
            "SUDO_UID": "1001", "LANG": "C", "LC_ALL": "C",
        }

    def state(self):
        return json.loads(self.state_path.read_text())

    def write_state(self, value):
        self.state_path.write_text(json.dumps(value))

    def update(self, **values):
        state = self.state()
        state.update(values)
        self.write_state(state)

    def trace(self):
        path = self.root / "trace.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def run(self, mode, **env):
        command = [BASH, "--noprofile", "--norc", "-c", 'source "$1"; DOCKER_ROOT="$2"; main "$3"',
                   "storage-test", str(SCRIPT), str(self.docker_root), mode]
        process = subprocess.Popen(
            command,
            cwd=self.root, env={**self.env, **env}, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=HELPER_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            # This session contains only our fixture shell and its descendants.
            # Killing Bash alone could leave stubs writing during fixture removal.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.communicate(timeout=HELPER_CLEANUP_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                pass
            finally:
                # communicate normally reaps the child. If draining timed out,
                # attempt only a nonblocking reap; never enter Popen.__exit__'s
                # unbounded wait or extend the five-second cleanup deadline.
                try:
                    process.wait(timeout=0)
                except subprocess.TimeoutExpired:
                    pass
                for pipe in (process.stdout, process.stderr):
                    if pipe is not None:
                        pipe.close()
            raise
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


class StorageHarnessProcessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rxls-storage-process-test-")
        self.addCleanup(self.temporary.cleanup)
        self.harness = StorageHarness(Path(self.temporary.name))
        self.process = mock.MagicMock(pid=43210, returncode=0)
        self.process.__enter__.return_value = self.process
        self.process.poll.return_value = 0
        self.process.communicate.return_value = ("fixture-output", "fixture-error")

    def test_dispatcher_accepts_python_interpreter_path_with_spaces(self):
        alias_directory = self.harness.root / "interpreter with spaces"
        alias_directory.mkdir()
        alias = alias_directory / "python3"
        alias.symlink_to(sys.executable)
        fixture = self.harness.root / "space-free-fixture"
        fixture.mkdir()
        with mock.patch.object(sys, "executable", str(alias)):
            harness = StorageHarness(fixture)
        self.assertNotIn(" ", str(harness.runner_temp))
        self.assertEqual(os.readlink(harness.bin / "python3"), str(alias))
        result = harness.run("prepare", GITHUB_ACTIONS="false")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("oracle_storage_error:hosted_context", result.stderr)
        self.assertIn(["id", "-u"], harness.trace())
        self.assertFalse([row for row in harness.trace() if row[0] in {"truncate", "mkfs.ext4", "mount", "systemctl"}])

    def test_transaction_uses_isolated_session_and_sixty_second_deadline(self):
        with mock.patch.object(subprocess, "Popen", return_value=self.process) as spawn, mock.patch.object(os, "killpg") as kill_group:
            result = self.harness.run("prepare")
        self.assertTrue(spawn.call_args.kwargs.get("start_new_session"))
        self.process.communicate.assert_called_once_with(timeout=60)
        self.process.__enter__.assert_not_called()
        self.process.__exit__.assert_not_called()
        kill_group.assert_not_called()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "fixture-output", "fixture-error"))
        self.assertEqual(result.args, spawn.call_args.args[0])

    def test_timeout_kills_only_own_group_and_reaps_with_bounded_cleanup(self):
        original = subprocess.TimeoutExpired(["fixture-helper"], 60, output="partial")
        self.process.communicate.side_effect = [original, ("drained", "error")]
        with mock.patch.object(subprocess, "Popen", return_value=self.process), mock.patch.object(os, "killpg") as kill_group:
            with self.assertRaises(subprocess.TimeoutExpired) as raised:
                self.harness.run("prepare")
        self.assertIs(raised.exception, original)
        kill_group.assert_called_once_with(self.process.pid, signal.SIGKILL)
        self.assertEqual(self.process.communicate.call_args_list, [mock.call(timeout=60), mock.call(timeout=5)])
        self.process.wait.assert_called_once_with(timeout=0)
        self.process.stdout.close.assert_called_once_with()
        self.process.stderr.close.assert_called_once_with()
        self.process.__exit__.assert_not_called()

    def test_cleanup_timeout_cannot_replace_original_or_wait_without_bound(self):
        original = subprocess.TimeoutExpired(["fixture-helper"], 60)
        self.process.communicate.side_effect = [original, subprocess.TimeoutExpired(["cleanup"], 5)]
        self.process.wait.side_effect = subprocess.TimeoutExpired(["reap"], 0)
        with mock.patch.object(subprocess, "Popen", return_value=self.process), mock.patch.object(os, "killpg") as kill_group:
            with self.assertRaises(subprocess.TimeoutExpired) as raised:
                self.harness.run("restore")
        self.assertIs(raised.exception, original)
        kill_group.assert_called_once_with(self.process.pid, signal.SIGKILL)
        self.assertEqual(self.process.communicate.call_args_list, [mock.call(timeout=60), mock.call(timeout=5)])
        self.process.wait.assert_called_once_with(timeout=0)
        self.process.stdout.close.assert_called_once_with()
        self.process.stderr.close.assert_called_once_with()
        self.process.__exit__.assert_not_called()

    def test_already_exited_process_group_still_reaps_and_preserves_timeout(self):
        original = subprocess.TimeoutExpired(["fixture-helper"], 60)
        self.process.communicate.side_effect = [original, ("", "")]
        with mock.patch.object(subprocess, "Popen", return_value=self.process), mock.patch.object(os, "killpg", side_effect=ProcessLookupError) as kill_group:
            with self.assertRaises(subprocess.TimeoutExpired) as raised:
                self.harness.run("restore")
        self.assertIs(raised.exception, original)
        kill_group.assert_called_once_with(self.process.pid, signal.SIGKILL)
        self.process.wait.assert_called_once_with(timeout=0)
        self.process.__exit__.assert_not_called()


class RenderOracleBuildStorageTests(unittest.TestCase):
    def test_helper_exists(self) -> None:
        self.assertTrue(SCRIPT.is_file(), "the hosted storage helper must exist")

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rxls-storage-test-")
        self.addCleanup(self.temporary.cleanup)
        self.harness = StorageHarness(Path(self.temporary.name))

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_no_storage_mutations(self, trace):
        dangerous = {"truncate", "mkfs.ext4", "mount", "umount"}
        self.assertFalse([row for row in trace if row[0] in dangerous or row[:2] in (["systemctl", "start"], ["systemctl", "stop"])], trace)

    def test_invalid_context_fails_before_storage_changes(self):
        for key, value in (("GITHUB_ACTIONS", "false"), ("RUNNER_ENVIRONMENT", "self-hosted"), ("RUNNER_OS", "macOS")):
            with self.subTest(key=key):
                result = self.harness.run("prepare", **{key: value})
                self.assertNotEqual(result.returncode, 0)
        self.assert_no_storage_mutations(self.harness.trace())

    def test_nonroot_and_wrong_distribution_fail_before_storage_changes(self):
        for mutation in ({"uid": 501}, {"uid": 0, "release": "22.04"}, {"release": "24.04", "machine": "aarch64"}):
            self.harness.update(**mutation)
            self.assertNotEqual(self.harness.run("prepare").returncode, 0)
        self.assert_no_storage_mutations(self.harness.trace())

    def test_existing_containers_or_volumes_are_not_destroyed(self):
        for values in ({"containers": ["unrelated-container"]}, {"containers": [], "volumes": ["unrelated-volume"]}):
            self.harness.update(**values)
            result = self.harness.run("prepare")
            self.assertNotEqual(result.returncode, 0)
        self.assert_no_storage_mutations(self.harness.trace())

    def test_prepare_restore_and_repeated_restore(self):
        self.assert_success(self.harness.run("prepare"))
        state = self.harness.state()
        self.assertIn(str(self.harness.docker_root / "volumes"), state["mounts"])
        self.assertTrue(state["daemon_active"])
        trace = self.harness.trace()
        format_index = next(i for i, row in enumerate(trace) if row[0] == "mkfs.ext4")
        bind_index = next(i for i, row in enumerate(trace) if row[0] == "mount" and "--bind" in row)
        stop_index = next(i for i, row in enumerate(trace) if row[:2] == ["systemctl", "stop"])
        self.assertLess(format_index, bind_index)
        self.assertLess(stop_index, bind_index)
        format_command = trace[format_index]
        self.assertIn("lazy_itable_init=0,lazy_journal_init=0,hash_seed=" + SEED, format_command)
        self.assertEqual(format_command[format_command.index("-U") + 1], UUID)
        self.assertEqual(state["seed"], SEED)
        self.assertIn(["flock", "-w", "5", "9"], trace)
        self.assert_success(self.harness.run("restore"))
        self.assertEqual(self.harness.state()["mounts"], {})
        self.assertIsNone(self.harness.state()["loop_image"])
        self.assertTrue(self.harness.state()["daemon_active"])
        self.assertEqual((self.harness.docker_root / "volumes" / "metadata.db").read_bytes(), b"original-metadata")
        self.assert_success(self.harness.run("restore"))

    def test_repeated_restore_allows_legitimate_original_metadata_rewrite(self):
        self.harness.update(mutate_original_metadata_on_start=True)
        self.assert_success(self.harness.run("prepare"))
        receipt = self.harness.job_state / "original-metadata.sha256"
        original_receipt = receipt.read_bytes()
        for attempt in range(2):
            with self.subTest(attempt=attempt):
                start = len(self.harness.trace())
                result = self.harness.run("restore")
                self.assert_success(result)
                self.assertNotIn("original_metadata_changed", result.stdout + result.stderr)
                self.assertEqual(receipt.read_bytes(), original_receipt)
                if attempt == 1:
                    calls = self.harness.trace()[start:]
                    self.assertFalse([row for row in calls if row[:2] in (["systemctl", "stop"], ["systemctl", "start"])
                                      or (row[0] == "sha256sum" and "--check" in row)])
        self.assertIn(b"legitimate-docker-startup", (self.harness.docker_root / "volumes" / "metadata.db").read_bytes())
        self.assertEqual(self.harness.state()["mounts"], {})
        self.assertIsNone(self.harness.state()["loop_image"])

    def test_failed_bind_rollback_allows_legitimate_original_metadata_rewrite(self):
        metadata = self.harness.docker_root / "volumes" / "metadata.db"
        expected_receipt = (hashlib.sha256(metadata.read_bytes()).hexdigest() + "  " + str(metadata) + "\n").encode()
        self.harness.update(mutate_original_metadata_on_start=True, failures={"mount_bind": 1})
        result = self.harness.run("prepare")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("oracle_storage_error:volumes_bind", result.stderr)
        self.assertIn("oracle_storage_rollback_status:0", result.stderr)
        self.assertNotIn("original_metadata_changed", result.stdout + result.stderr)
        self.assertEqual(self.harness.state()["mounts"], {})
        self.assertIsNone(self.harness.state()["loop_image"])
        self.assertTrue(self.harness.state()["daemon_active"])
        receipt = self.harness.job_state / "original-metadata.sha256"
        original_receipt = receipt.read_bytes()
        self.assertEqual(original_receipt, expected_receipt)
        restored = self.harness.run("restore")
        self.assert_success(restored)
        self.assertNotIn("original_metadata_changed", restored.stdout + restored.stderr)
        self.assertEqual(receipt.read_bytes(), original_receipt)

    def test_restored_state_rejects_foreign_mount_backing_loop_and_bad_result(self):
        self.assert_success(self.harness.run("prepare"))
        self.assert_success(self.harness.run("restore"))
        baseline = self.harness.state()
        image = self.harness.job_state / "canonical.ext4"
        mount = self.harness.job_state / "mount"
        result_file = self.harness.job_state / "result"
        result_bytes = result_file.read_bytes()
        receipt = self.harness.job_state / "original-metadata.sha256"
        receipt_bytes = receipt.read_bytes()
        marker = self.harness.job_state / "original-metadata-verified"
        marker_bytes = marker.read_bytes()
        for case in ("foreign_volumes_mount", "foreign_image_mount", "foreign_backing", "lingering_loop", "malformed_result", "malformed_marker", "missing_marker"):
            with self.subTest(case=case):
                state = json.loads(json.dumps(baseline))
                result_file.write_bytes(result_bytes)
                marker.write_bytes(marker_bytes)
                if case in {"foreign_volumes_mount", "foreign_image_mount"}:
                    target = self.harness.docker_root / "volumes" if case == "foreign_volumes_mount" else mount
                    target.mkdir(exist_ok=True)
                    state["mounts"][str(target)] = {"SOURCE": "/dev/loop99", "FSTYPE": "ext4", "FSROOT": "/foreign", "MAJ:MIN": "7:99", "TARGET": str(target)}
                elif case == "foreign_backing":
                    image.write_bytes(b"unrelated-backing")
                    state.setdefault("image_sizes", {})[str(image)] = 12884901888
                    state["loop_image"] = str(image)
                    state["foreign_backing"] = str(self.harness.root / "foreign.img")
                elif case == "lingering_loop":
                    state["loop_image"] = str(image)
                elif case == "malformed_result":
                    result_file.write_bytes(b"not-a-valid-result\n")
                elif case == "malformed_marker":
                    marker.write_bytes(b"not-an-owned-marker\n")
                else:
                    marker.unlink()
                self.harness.write_state(state)
                start = len(self.harness.trace())
                try:
                    result = self.harness.run("restore")
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    calls = self.harness.trace()[start:]
                    self.assert_no_storage_mutations(calls)
                    self.assertFalse([row for row in calls if row[0] in {"rm", "rmdir"} or row[:2] == ["losetup", "--detach"]])
                    self.assertEqual(self.harness.state()["mounts"], state["mounts"])
                    self.assertEqual(self.harness.state()["loop_image"], state["loop_image"])
                    self.assertEqual(receipt.read_bytes(), receipt_bytes)
                    if case == "foreign_backing":
                        self.assertEqual(image.read_bytes(), b"unrelated-backing")
                finally:
                    image.unlink(missing_ok=True)
                    if mount.exists():
                        mount.rmdir()

    def test_first_restore_still_rejects_changed_original_metadata(self):
        self.assert_success(self.harness.run("prepare"))
        metadata = self.harness.docker_root / "volumes" / "metadata.db"
        metadata.write_bytes(b"unexpected-original-store-change")
        start = len(self.harness.trace())
        result = self.harness.run("restore")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("original_metadata_changed", result.stderr)
        self.assertFalse((self.harness.job_state / "original-metadata-verified").exists())
        self.assertTrue((self.harness.job_state / "canonical.ext4").exists())
        self.assertFalse([row for row in self.harness.trace()[start:] if row[:2] == ["systemctl", "start"] or row[0] == "rm"])

    def test_prepare_failure_rolls_back_and_restore_is_safe(self):
        for event in ("truncate", "truncate_partial", "mkfs.ext4", "loop_attach", "mount_loop", "order_probe", "mount_bind", "systemctl_start"):
            with self.subTest(event=event), tempfile.TemporaryDirectory(prefix="rxls-storage-failure-") as raw:
                harness = StorageHarness(Path(raw))
                harness.update(failures={event: 1})
                result = harness.run("prepare")
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(harness.state()["mounts"], {}, result.stdout + result.stderr)
                self.assertIsNone(harness.state()["loop_image"])
                self.assertTrue(harness.state()["daemon_active"])
                self.assertFalse((harness.job_state / "canonical.ext4").exists())
                self.assertIn("oracle_storage_rollback_status:0", result.stderr)
                self.assert_success(harness.run("restore"))

    def test_failed_inventory_queries_fail_closed(self):
        for event in ("container_inventory", "volume_inventory"):
            with self.subTest(event=event), tempfile.TemporaryDirectory(prefix="rxls-storage-inventory-") as raw:
                harness = StorageHarness(Path(raw))
                harness.update(failures={event: 1})
                result = harness.run("prepare")
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_no_storage_mutations(harness.trace())
                self.assertFalse(harness.job_state.exists())

    def test_restore_inventory_and_lock_failures_preserve_backing(self):
        self.assert_success(self.harness.run("prepare"))
        for event in ("container_inventory", "volume_inventory", "daemon_inventory", "loop_inventory", "flock"):
            with self.subTest(event=event):
                self.harness.update(failures={event: 1})
                start = len(self.harness.trace())
                result = self.harness.run("restore")
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_no_storage_mutations(self.harness.trace()[start:])
                self.assertTrue((self.harness.job_state / "canonical.ext4").exists())
                self.assertIn(str(self.harness.docker_root / "volumes"), self.harness.state()["mounts"])

    def test_wrong_directory_order_rolls_back_before_binding(self):
        self.harness.update(wrong_order=True)
        result = self.harness.run("prepare")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("oracle_storage_error:directory_order", result.stderr)
        self.assertIn("oracle_storage_rollback_status:0", result.stderr)
        self.assertFalse([row for row in self.harness.trace() if row[0] == "mount" and "--bind" in row])
        self.assertEqual(self.harness.state()["mounts"], {})
        self.assertIsNone(self.harness.state()["loop_image"])

    def test_task_inventory_failure_rolls_back_without_bind(self):
        self.harness.update(failures={"task_inventory": 1})
        result = self.harness.run("prepare")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("oracle_storage_error:containerd_query", result.stderr)
        self.assertIn("oracle_storage_rollback_status:0", result.stderr)
        self.assertFalse([row for row in self.harness.trace() if row[0] == "mount" and "--bind" in row])
        self.assertEqual(self.harness.state()["mounts"], {})
        self.assertTrue(self.harness.state()["daemon_active"])

    def test_termination_during_prepare_uses_owned_state_rollback(self):
        self.harness.update(signal_on="mount_bind")
        result = self.harness.run("prepare")
        self.assertEqual(result.returncode, 143, result.stdout + result.stderr)
        self.assertIn("oracle_storage_rollback_status:0", result.stderr)
        self.assertEqual(self.harness.state()["mounts"], {})
        self.assertIsNone(self.harness.state()["loop_image"])
        self.assertTrue(self.harness.state()["daemon_active"])

    def test_restore_refuses_live_container_without_unmount(self):
        self.assert_success(self.harness.run("prepare"))
        self.harness.update(containers=["still-running"])
        start = len(self.harness.trace())
        result = self.harness.run("restore")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse([row for row in self.harness.trace()[start:] if row[0] == "umount"])
        self.assertIn(str(self.harness.docker_root / "volumes"), self.harness.state()["mounts"])

    def test_inactive_daemon_still_refuses_persisted_unrelated_volume(self):
        self.assert_success(self.harness.run("prepare"))
        self.harness.update(daemon_active=False, socket_active=False, volumes=["unrelated-volume"])
        start = len(self.harness.trace())
        result = self.harness.run("restore")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse([row for row in self.harness.trace()[start:] if row[0] in {"umount", "rm"}], result.stdout + result.stderr)
        self.assertIn(str(self.harness.docker_root / "volumes"), self.harness.state()["mounts"])
        self.assertTrue((self.harness.job_state / "canonical.ext4").exists())

    def test_busy_unmount_fails_without_lazy_detach_or_image_deletion(self):
        self.assert_success(self.harness.run("prepare"))
        before = self.harness.state()
        self.harness.update(failures={"umount_bind": 1})
        result = self.harness.run("restore")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(Path(before["loop_image"]).is_file())
        self.assertIn(str(self.harness.docker_root / "volumes"), self.harness.state()["mounts"])
        for row in self.harness.trace():
            if row[0] == "umount":
                self.assertFalse(set(row[1:]) & {"-l", "--lazy", "-f", "--force"})

    def test_foreign_backing_file_refuses_teardown(self):
        self.assert_success(self.harness.run("prepare"))
        self.harness.update(foreign_backing=str(self.harness.root / "not-owned.img"))
        start = len(self.harness.trace())
        self.assertNotEqual(self.harness.run("restore").returncode, 0)
        self.assertFalse([row for row in self.harness.trace()[start:] if row[0] == "umount"])

    def test_preexisting_state_symlink_is_rejected(self):
        unrelated = self.harness.root / "unrelated"
        unrelated.mkdir()
        sentinel = unrelated / "sentinel"
        sentinel.write_text("untouched")
        self.harness.job_state.symlink_to(unrelated, target_is_directory=True)
        self.assertNotEqual(self.harness.run("prepare").returncode, 0)
        self.assertEqual(sentinel.read_text(), "untouched")
        self.assert_no_storage_mutations(self.harness.trace())

    def test_missing_owned_state_refuses_restore_while_volumes_remain_bound(self):
        self.assert_success(self.harness.run("prepare"))
        relocated = self.harness.runner_temp / "relocated-owned-state"
        self.harness.job_state.rename(relocated)
        self.addCleanup(relocated.rename, self.harness.job_state)
        start = len(self.harness.trace())
        before = self.harness.state()
        result = self.harness.run("restore")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.harness.trace()[start:]
        self.assert_no_storage_mutations(calls)
        self.assertFalse([row for row in calls if row[0] in {"rm", "rmdir"} or row[:2] == ["losetup", "--detach"]])
        self.assertEqual(self.harness.state()["mounts"], before["mounts"])
        self.assertEqual(self.harness.state()["loop_image"], before["loop_image"])
        self.assertTrue((relocated / "canonical.ext4").is_file())


if __name__ == "__main__":
    unittest.main()
