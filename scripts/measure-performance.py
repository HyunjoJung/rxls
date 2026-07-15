#!/usr/bin/env python3
"""Measure rxls diagnose or package-preserving edit/save resource use."""

from __future__ import annotations

import argparse
import json
import os
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


def resident_kib(pid: int) -> int | None:
    status = Path(f"/proc/{pid}/status")
    if status.is_file():
        for line in status.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith(("VmHWM:", "VmRSS:")):
                fields = line.split()
                if len(fields) >= 2 and fields[1].isdigit():
                    return int(fields[1])
    try:
        output = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            check=False,
            capture_output=True,
            text=True,
        ).stdout.strip()
        value = int(output) if output.isdigit() else 0
        return value if value > 0 else None
    except OSError:
        return None


def child_rusage_peak_kib() -> int | None:
    """Return the OS child-process peak for conservative RSS accounting."""
    try:
        import resource

        value = int(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
        if sys.platform == "darwin":
            value //= 1024  # macOS reports bytes; Linux and BSD report KiB.
        return value if value > 0 else None
    except (ImportError, ValueError):
        return None


def conservative_peak_rss_kib(
    sampled_peak: int | None, child_peak: int | None
) -> tuple[int | None, str | None]:
    """Combine polling and OS child-rusage peaks without under-reporting RSS."""
    if sampled_peak is not None and child_peak is not None:
        return max(sampled_peak, child_peak), "sampled+child-rusage"
    if sampled_peak is not None:
        return sampled_peak, "sampled"
    if child_peak is not None:
        return child_peak, "child-rusage-fallback"
    return None, None


def _terminate_process_tree(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
    except ProcessLookupError:
        pass


def measure(command: list[str], timeout: float | None = None) -> dict[str, object]:
    started = time.perf_counter()
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=os.name == "posix",
    )
    samples: list[int] = []
    stop_sampling = threading.Event()

    def sample_memory() -> None:
        while not stop_sampling.is_set():
            sample = resident_kib(process.pid)
            if sample is not None:
                samples.append(sample)
            if process.poll() is not None:
                break
            stop_sampling.wait(0.01)

    sampler = threading.Thread(target=sample_memory, name="rxls-rss-sampler", daemon=True)
    sampler.start()
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        _terminate_process_tree(process)
        stdout, stderr = process.communicate()
    finally:
        stop_sampling.set()
        sampler.join(timeout=1)
    elapsed = time.perf_counter() - started
    if not timed_out and process.returncode != 0:
        raise RuntimeError(
            f"command failed with {process.returncode}: "
            + stderr.decode("utf-8", errors="replace")
        )
    sampled_peak = max(samples) if samples else None
    child_peak = child_rusage_peak_kib()
    peak_kib, rss_source = conservative_peak_rss_kib(sampled_peak, child_peak)
    return {
        "seconds": round(elapsed, 6),
        "peak_rss_bytes": peak_kib * 1024 if peak_kib is not None else None,
        "rss_source": rss_source,
        "stdout_bytes": len(stdout),
        "timed_out": timed_out,
    }


def evidence_path(path: Path) -> str:
    """Render an input path without embedding the checkout or runner home."""
    resolved = path.resolve()
    try:
        return resolved.relative_to(Path.cwd().resolve()).as_posix()
    except ValueError:
        return resolved.name


def operation_samples(
    binary: Path,
    operation: str,
    input_path: Path,
    repeat: int,
    timeout: float | None,
) -> list[dict[str, object]]:
    """Collect repeated measurements and operation-specific output sizes."""
    samples: list[dict[str, object]] = []
    with tempfile.TemporaryDirectory(prefix="rxls-performance-") as tmp:
        output_root = Path(tmp)
        for index in range(repeat):
            if operation == "diagnose":
                command = [str(binary), "diagnose", str(input_path)]
                output_path = None
            else:
                output_path = output_root / f"edited-{index}.xlsx"
                command = [str(binary), str(input_path), str(output_path)]
            sample = measure(command, timeout=timeout)
            if output_path is None:
                sample["output_bytes"] = int(sample["stdout_bytes"])
            elif output_path.is_file():
                sample["output_bytes"] = output_path.stat().st_size
            elif not bool(sample["timed_out"]):
                raise RuntimeError("edit-save command did not create its output workbook")
            else:
                sample["output_bytes"] = None
            samples.append(sample)
    return samples


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, default=Path("target/release/rxls"))
    parser.add_argument(
        "--operation",
        choices=("diagnose", "edit-save"),
        default="diagnose",
    )
    parser.add_argument("--case", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--max-seconds", type=float)
    parser.add_argument("--max-rss-mib", type=float)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)

    try:
        if args.repeat < 1:
            raise ValueError("--repeat must be positive")
        if args.max_seconds is not None and args.max_seconds <= 0:
            raise ValueError("--max-seconds must be positive")
        if args.max_rss_mib is not None and args.max_rss_mib <= 0:
            raise ValueError("--max-rss-mib must be positive")
        cases = []
        failed = False
        for value in args.case:
            label, separator, raw_path = value.partition("=")
            if not separator or not label or not raw_path:
                raise ValueError("--case must be LABEL=PATH")
            path = Path(raw_path)
            if not path.is_file():
                raise FileNotFoundError(path)
            samples = operation_samples(
                args.bin,
                args.operation,
                path,
                args.repeat,
                args.max_seconds,
            )
            seconds = [float(sample["seconds"]) for sample in samples]
            rss = [
                int(sample["peak_rss_bytes"])
                for sample in samples
                if sample["peak_rss_bytes"] is not None
            ]
            rss_sampling_complete = len(rss) == len(samples)
            output_sizes = [
                int(sample["output_bytes"])
                for sample in samples
                if sample["output_bytes"] is not None
            ]
            output_sampling_complete = len(output_sizes) == len(samples)
            case = {
                "label": label,
                "path": evidence_path(path),
                "input_bytes": path.stat().st_size,
                "output_bytes": max(output_sizes) if output_sizes else None,
                "output_size_consistent": (
                    output_sampling_complete and len(set(output_sizes)) == 1
                ),
                "repeats": args.repeat,
                "median_seconds": round(statistics.median(seconds), 6),
                "max_seconds": max(seconds),
                "max_peak_rss_bytes": max(rss) if rss else None,
                "rss_sampling_complete": rss_sampling_complete,
                "samples": samples,
            }
            if any(bool(sample["timed_out"]) for sample in samples):
                failed = True
            if args.max_seconds is not None and case["max_seconds"] > args.max_seconds:
                failed = True
            if args.max_rss_mib is not None and not rss_sampling_complete:
                failed = True
            if not bool(case["output_size_consistent"]):
                failed = True
            if (
                args.max_rss_mib is not None
                and case["max_peak_rss_bytes"] is not None
                and case["max_peak_rss_bytes"] > args.max_rss_mib * 1024 * 1024
            ):
                failed = True
            cases.append(case)
        payload = {
            "schema": "rxls.performance-evidence.v1",
            "command": args.operation,
            "budgets": {
                "max_seconds": args.max_seconds,
                "max_rss_mib": args.max_rss_mib,
            },
            "passed": not failed,
            "cases": cases,
        }
        rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered, encoding="utf-8")
        else:
            sys.stdout.write(rendered)
        return 1 if failed else 0
    except (OSError, RuntimeError, ValueError) as error:
        print(f"measure-performance: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
