"""Own ordinary command descendants, without names or system process scans."""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

GATE = b"rxls-owned-command\n"
WINDOWS = os.name == "nt"


class WindowsJob:
    """Unnamed parent-owned kill-on-close job; breakaway is not enabled.

    https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
    """

    def __init__(self):
        class Limits(ctypes.Structure):
            _fields_ = [("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64), ("flags", wintypes.DWORD), ("min_ws", ctypes.c_size_t), ("max_ws", ctypes.c_size_t), ("processes", wintypes.DWORD), ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD), ("scheduling", wintypes.DWORD)]

        class Extended(ctypes.Structure):
            _fields_ = [("basic", Limits), ("io", ctypes.c_uint64 * 6), ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t), ("peak_process", ctypes.c_size_t), ("peak_job", ctypes.c_size_t)]

        api = ctypes.WinDLL("kernel32", use_last_error=True)
        signatures = {
            "CreateJobObjectW": ([ctypes.c_void_p, wintypes.LPCWSTR], wintypes.HANDLE),
            "SetInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD], wintypes.BOOL),
            "AssignProcessToJobObject": ([wintypes.HANDLE, wintypes.HANDLE], wintypes.BOOL),
            "TerminateJobObject": ([wintypes.HANDLE, wintypes.UINT], wintypes.BOOL),
            "QueryInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.c_void_p], wintypes.BOOL),
            "CloseHandle": ([wintypes.HANDLE], wintypes.BOOL),
        }
        for name, (arguments, result) in signatures.items():
            getattr(api, name).argtypes, getattr(api, name).restype = arguments, result
        self.api = api
        self.observations = []
        self.handle = api.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = Extended()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not api.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            error = ctypes.WinError(ctypes.get_last_error())
            api.CloseHandle(self.handle)
            self.handle = None
            raise error

    def assign(self, process):
        # Popen's retained native process handle cannot identify a reused PID.
        if not self.api.AssignProcessToJobObject(self.handle, wintypes.HANDLE(int(process._handle))):
            raise ctypes.WinError(ctypes.get_last_error())
        self.active_processes("assigned-before-gate")

    def active_processes(self, phase):
        class Accounting(ctypes.Structure):
            _fields_ = [("times", ctypes.c_int64 * 4), ("page_faults", wintypes.DWORD), ("total_processes", wintypes.DWORD), ("active_processes", wintypes.DWORD), ("terminated_processes", wintypes.DWORD)]

        value = Accounting()
        if not self.api.QueryInformationJobObject(self.handle, 1, ctypes.byref(value), ctypes.sizeof(value), None):
            raise ctypes.WinError(ctypes.get_last_error())
        if len(self.observations) < 128:
            self.observations.append({"phase": phase, "active_processes": value.active_processes})
        return value.active_processes

    def kill(self):
        if not self.handle:
            return
        self.active_processes("before-terminate")
        if self.handle and not self.api.TerminateJobObject(self.handle, 1):
            raise ctypes.WinError(ctypes.get_last_error())
        deadline = time.monotonic() + 10
        while self.active_processes("after-terminate"):
            if time.monotonic() >= deadline:
                raise OSError("owned job members did not exit")
            time.sleep(0.1)

    def close(self):
        if self.handle:
            primary = None
            try:
                if self.active_processes("before-close"):
                    self.kill()
            except Exception as error:
                primary = error
            if not self.api.CloseHandle(self.handle):
                cleanup = ctypes.WinError(ctypes.get_last_error())
                if primary:
                    raise ValueError(f"owned job cleanup: {primary}; handle close: {cleanup}") from primary
                raise cleanup
            self.handle = None
            if primary:
                raise primary


class OwnedCommand:
    @property
    def observations(self):
        return self.job.observations if self.job else []

    def __init__(self, command, cwd, env=None):
        self.job = None
        self.process = None
        if WINDOWS:
            encoded = json.dumps(command)
            if len(encoded.encode("utf-8")) > 16 * 1024:
                raise ValueError("owned command argument limit")
            self.job = WindowsJob()
            try:
                self.process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--command-worker", encoded], cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, creationflags=0x08000000)  # CREATE_NO_WINDOW
                # Worker cannot spawn the command until job assignment succeeds.
                self.job.assign(self.process)
                self.process.stdin.write(GATE)
                self.process.stdin.flush()
                self.process.stdin.close()
            except BaseException as primary:
                errors = []
                try:
                    self.job.kill()
                except Exception as error:
                    errors.append(str(error))
                try:
                    if self.process is not None:
                        self.process.kill()  # Covers an unassigned gated worker.
                        self.process.wait(timeout=10)
                except Exception as error:
                    errors.append(str(error))
                finally:
                    try:
                        self.job.close()
                    except Exception as error:
                        errors.append(str(error))
                failure = ValueError(f"owned command start: {primary}; cleanup: {errors}") if errors else primary
                failure.owned_exit_code = getattr(self.process, "returncode", None)
                failure.owned_observations = list(getattr(self.job, "observations", []))[:128]
                if errors:
                    raise failure from primary
                raise
        else:
            self.process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)

    def kill(self):
        if self.job:
            self.job.kill()
        else:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def close(self):
        if self.job:
            self.job.close()  # Also stops any remaining inherited descendants.
        else:
            # The leader may have exited while a descendant still owns stdout.
            self.kill()
        if self.process.poll() is None:
            self.process.wait(timeout=10)


def worker(arguments, stream=None) -> int:
    if len(arguments) != 1 or len(arguments[0].encode("utf-8")) > 16 * 1024:
        raise ValueError("owned worker arguments")
    command = json.loads(arguments[0])
    if not isinstance(command, list) or not command or any(not isinstance(value, str) or not value or "\0" in value for value in command):
        raise ValueError("owned worker command shape")
    if (stream or sys.stdin.buffer).readline(32) != GATE:
        raise ValueError("owned worker assignment gate absent")
    return subprocess.call(command, stdin=subprocess.DEVNULL, creationflags=0x08000000 if os.name == "nt" else 0)


if __name__ == "__main__":
    try:
        if sys.argv[1:2] != ["--command-worker"]:
            raise ValueError("internal owned command worker only")
        raise SystemExit(worker(sys.argv[2:]))
    except (OSError, ValueError) as error:
        print(f"owned command: {error}", file=sys.stderr)
        raise SystemExit(1)
