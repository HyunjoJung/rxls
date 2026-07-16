#!/usr/bin/env python3
"""Collect path-neutral, bounded renderer performance evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import signal
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Sequence


SCHEMA = "rxls.render-performance-evidence.v1"
DRIVER_SCHEMA = "rxls.render-performance-driver.v1"
BUDGET_SCHEMA = "rxls.render-performance-budgets.v1"
MAX_CAPTURE_BYTES = 64 << 10
SHA256_RE = re.compile(r"[0-9a-f]{64}")
FONT_PACK_CASES = {
    "wrapped-cjk",
    "many-styles",
    "merge-grid",
    "hundreds-of-pages",
}
DRIVER_KEYS = {
    "artifact_sha256",
    "backend_commands",
    "case",
    "disposition",
    "limit_kind",
    "output_bytes",
    "pages",
    "schema",
}
DEFAULT_BUDGETS: dict[str, dict[str, int | float]] = {
    "huge-sparse-sheet": {
        "max_wall_seconds": 5.0,
        "max_rss_bytes": 512 << 20,
        "min_pages": 1,
        "max_pages": 1,
        "max_backend_commands": 100,
        "max_output_bytes": 1 << 20,
    },
    "wrapped-cjk": {
        "max_wall_seconds": 10.0,
        "max_rss_bytes": 768 << 20,
        "min_pages": 1,
        "max_pages": 1,
        "max_backend_commands": 600_000,
        "max_output_bytes": 32 << 20,
    },
    "many-styles": {
        "max_wall_seconds": 10.0,
        "max_rss_bytes": 768 << 20,
        "min_pages": 1,
        "max_pages": 1,
        # Calibrated against the complete locked Latin substitution families
        # (Arimo/Caladea/Carlito/Cousine/Tinos), whose real outlines are more
        # detailed than the earlier CJK fallback. These remain well below the
        # renderer's independent scene-command and SVG hard ceilings.
        "max_backend_commands": 450_000,
        "max_output_bytes": 24 << 20,
    },
    "merge-grid": {
        "max_wall_seconds": 10.0,
        "max_rss_bytes": 768 << 20,
        "min_pages": 1,
        "max_pages": 1,
        "max_backend_commands": 300_000,
        "max_output_bytes": 16 << 20,
    },
    "hundreds-of-pages": {
        "max_wall_seconds": 20.0,
        "max_rss_bytes": 1 << 30,
        "min_pages": 100,
        "max_pages": 512,
        "max_backend_commands": 1_000_000,
        "max_output_bytes": 32 << 20,
    },
    "image-bomb-headers": {
        "max_wall_seconds": 5.0,
        "max_rss_bytes": 512 << 20,
        "min_pages": 0,
        "max_pages": 0,
        "max_backend_commands": 0,
        "max_output_bytes": 0,
    },
    "image-pixel-limits": {
        "max_wall_seconds": 5.0,
        "max_rss_bytes": 512 << 20,
        "min_pages": 0,
        "max_pages": 0,
        "max_backend_commands": 0,
        "max_output_bytes": 0,
    },
    "decoded-media-limits": {
        "max_wall_seconds": 5.0,
        "max_rss_bytes": 512 << 20,
        "min_pages": 0,
        "max_pages": 0,
        "max_backend_commands": 0,
        "max_output_bytes": 0,
    },
    "chart-point-limits": {
        "max_wall_seconds": 5.0,
        "max_rss_bytes": 512 << 20,
        "min_pages": 0,
        "max_pages": 0,
        "max_backend_commands": 0,
        "max_output_bytes": 0,
    },
}


class GateError(RuntimeError):
    """A malformed driver response or invalid gate configuration."""


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical_sha256(value: object) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return sha256_bytes(encoded)


def resident_kib(pid: int) -> int | None:
    status = Path(f"/proc/{pid}/status")
    if status.is_file():
        for line in status.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith(("VmHWM:", "VmRSS:")):
                fields = line.split()
                if len(fields) >= 2 and fields[1].isdigit():
                    return int(fields[1])
    try:
        completed = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            check=False,
            capture_output=True,
            text=True,
            timeout=1,
        )
        value = completed.stdout.strip()
        return int(value) if value.isdigit() and int(value) > 0 else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def child_rusage_peak_kib() -> int | None:
    try:
        import resource

        value = int(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
        if sys.platform == "darwin":
            value //= 1024
        return value if value > 0 else None
    except (ImportError, ValueError):
        return None


def conservative_peak_rss_kib(
    sampled_peak: int | None, child_peak: int | None
) -> tuple[int | None, str | None]:
    if sampled_peak is not None and child_peak is not None:
        return max(sampled_peak, child_peak), "sampled+child-rusage"
    if sampled_peak is not None:
        return sampled_peak, "sampled"
    if child_peak is not None:
        return child_peak, "child-rusage-fallback"
    return None, None


def _kill_process_tree(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
    except ProcessLookupError:
        pass


def measure_process(
    command: list[str], timeout: float, *, env: dict[str, str] | None = None
) -> dict[str, object]:
    started = time.perf_counter()
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=os.name == "posix",
        env=env,
    )
    rss_samples: list[int] = []
    stop = threading.Event()

    def sample_memory() -> None:
        while not stop.is_set():
            sample = resident_kib(process.pid)
            if sample is not None:
                rss_samples.append(sample)
            if process.poll() is not None:
                break
            stop.wait(0.005)

    sampler = threading.Thread(target=sample_memory, name="render-rss", daemon=True)
    sampler.start()
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        _kill_process_tree(process)
        stdout, stderr = process.communicate()
    finally:
        stop.set()
        sampler.join(timeout=1)
    elapsed = round(time.perf_counter() - started, 6)
    sampled_peak = max(rss_samples) if rss_samples else None
    peak_kib, rss_source = conservative_peak_rss_kib(
        sampled_peak, child_rusage_peak_kib()
    )
    return {
        "returncode": process.returncode,
        "timed_out": timed_out,
        "wall_seconds": elapsed,
        "peak_rss_bytes": peak_kib * 1024 if peak_kib is not None else None,
        "rss_source": rss_source,
        "stdout": stdout[: MAX_CAPTURE_BYTES + 1],
        "stderr": stderr[: MAX_CAPTURE_BYTES + 1],
        "stdout_truncated": len(stdout) > MAX_CAPTURE_BYTES,
        "stderr_truncated": len(stderr) > MAX_CAPTURE_BYTES,
    }


def parse_driver_payload(data: bytes, expected_case: str) -> dict[str, object]:
    if len(data) > MAX_CAPTURE_BYTES:
        raise GateError("driver stdout exceeded the capture limit")
    try:
        payload = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError("driver stdout was not one JSON object") from error
    if not isinstance(payload, dict) or set(payload) != DRIVER_KEYS:
        raise GateError("driver response fields differ from the strict schema")
    if payload.get("schema") != DRIVER_SCHEMA or payload.get("case") != expected_case:
        raise GateError("driver response identity differs")
    for field in ("pages", "backend_commands", "output_bytes"):
        value = payload.get(field)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise GateError(f"driver {field} must be a non-negative integer")
    digest = payload.get("artifact_sha256")
    if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
        raise GateError("driver artifact_sha256 is invalid")
    if payload.get("disposition") not in {"rendered", "bounded_limit"}:
        raise GateError("driver disposition is invalid")
    limit_kind = payload.get("limit_kind")
    if limit_kind is not None and not isinstance(limit_kind, str):
        raise GateError("driver limit_kind is invalid")
    return payload


def run_sample(
    driver: Path,
    case: str,
    budget: dict[str, int | float],
    font_pack_manifest: Path | None = None,
) -> dict[str, object]:
    environment = os.environ.copy()
    if font_pack_manifest is not None:
        environment["RXLS_RENDER_FONT_PACK_MANIFEST"] = str(font_pack_manifest)
    measured = measure_process(
        [str(driver), case],
        float(budget["max_wall_seconds"]),
        env=environment,
    )
    sample: dict[str, object] = {
        "wall_seconds": measured["wall_seconds"],
        "peak_rss_bytes": measured["peak_rss_bytes"],
        "rss_source": measured["rss_source"],
        "timed_out": measured["timed_out"],
        "returncode": measured["returncode"],
        "stdout_bytes": len(measured["stdout"]),
        "stderr_bytes": len(measured["stderr"]),
        "capture_complete": not measured["stdout_truncated"]
        and not measured["stderr_truncated"],
    }
    if (
        not bool(measured["timed_out"])
        and measured["returncode"] == 0
        and bool(sample["capture_complete"])
    ):
        try:
            sample["metrics"] = parse_driver_payload(
                measured["stdout"],  # type: ignore[arg-type]
                case,
            )
        except GateError as error:
            sample["driver_error"] = str(error)
    return sample


def evaluate_case(
    case: str,
    budget: dict[str, int | float],
    samples: list[dict[str, object]],
) -> tuple[dict[str, object], bool]:
    violations: set[str] = set()
    metrics = [sample.get("metrics") for sample in samples]
    complete_metrics = [value for value in metrics if isinstance(value, dict)]
    if len(complete_metrics) != len(samples):
        violations.add("driver_failure")
    deterministic = bool(complete_metrics) and all(
        value == complete_metrics[0] for value in complete_metrics[1:]
    )
    if not deterministic:
        violations.add("nondeterministic_metrics")
    if any(bool(sample.get("timed_out")) for sample in samples):
        violations.add("wall_timeout")
    max_wall = max((float(sample["wall_seconds"]) for sample in samples), default=0.0)
    if max_wall > float(budget["max_wall_seconds"]):
        violations.add("wall_seconds")
    rss_values = [
        int(sample["peak_rss_bytes"])
        for sample in samples
        if sample.get("peak_rss_bytes") is not None
    ]
    rss_complete = len(rss_values) == len(samples)
    if not rss_complete:
        violations.add("rss_unavailable")
    max_rss = max(rss_values) if rss_values else None
    if max_rss is not None and max_rss > int(budget["max_rss_bytes"]):
        violations.add("rss_bytes")

    selected = complete_metrics[0] if deterministic else None
    if selected is not None:
        pages = int(selected["pages"])
        if pages < int(budget["min_pages"]):
            violations.add("minimum_pages")
        if pages > int(budget["max_pages"]):
            violations.add("pages")
        if int(selected["backend_commands"]) > int(budget["max_backend_commands"]):
            violations.add("backend_commands")
        if int(selected["output_bytes"]) > int(budget["max_output_bytes"]):
            violations.add("output_bytes")

    record = {
        "aggregate": {
            "max_peak_rss_bytes": max_rss,
            "max_wall_seconds": max_wall,
            "median_wall_seconds": round(
                statistics.median(float(sample["wall_seconds"]) for sample in samples),
                6,
            )
            if samples
            else None,
            "rss_sampling_complete": rss_complete,
        },
        "budget": dict(budget),
        "case": case,
        "deterministic_metrics": deterministic,
        "metrics": selected,
        "passed": not violations,
        "samples": samples,
        "violations": sorted(violations),
    }
    return record, not violations


def validate_budget(case: str, budget: object) -> dict[str, int | float]:
    expected = {
        "max_wall_seconds",
        "max_rss_bytes",
        "min_pages",
        "max_pages",
        "max_backend_commands",
        "max_output_bytes",
    }
    if not isinstance(budget, dict) or set(budget) != expected:
        raise GateError(f"budget fields differ for {case}")
    normalized: dict[str, int | float] = {}
    for field in expected:
        value = budget[field]
        if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
            raise GateError(f"budget {case}.{field} must be non-negative")
        if field != "max_wall_seconds" and not isinstance(value, int):
            raise GateError(f"budget {case}.{field} must be an integer")
        normalized[field] = value
    if normalized["max_wall_seconds"] <= 0 or normalized["max_rss_bytes"] <= 0:
        raise GateError(f"wall and RSS budgets must be positive for {case}")
    if normalized["min_pages"] > normalized["max_pages"]:
        raise GateError(f"minimum pages exceed maximum pages for {case}")
    return normalized


def load_budgets(path: Path | None) -> tuple[dict[str, dict[str, int | float]], str]:
    if path is None:
        budgets = {
            case: validate_budget(case, budget)
            for case, budget in DEFAULT_BUDGETS.items()
        }
        return budgets, canonical_sha256(
            {"schema": BUDGET_SCHEMA, "cases": budgets}
        )
    raw = path.read_bytes()
    payload = json.loads(raw)
    if not isinstance(payload, dict) or set(payload) != {"schema", "cases"}:
        raise GateError("budget file fields differ from the strict schema")
    if payload["schema"] != BUDGET_SCHEMA or not isinstance(payload["cases"], dict):
        raise GateError("budget file schema differs")
    if set(payload["cases"]) != set(DEFAULT_BUDGETS):
        raise GateError("budget file must define the exact workload set")
    budgets = {
        case: validate_budget(case, payload["cases"][case])
        for case in DEFAULT_BUDGETS
    }
    return budgets, sha256_bytes(raw)


def driver_identity(driver: Path) -> dict[str, str]:
    if not driver.is_file():
        raise GateError(f"driver does not exist: {driver.name}")
    completed = subprocess.run(
        [str(driver), "--version"],
        check=False,
        capture_output=True,
        timeout=5,
    )
    version = completed.stdout.decode("utf-8", errors="strict").strip()
    if completed.returncode != 0 or not version or len(version) > 128:
        raise GateError("driver version probe failed")
    return {
        "name": driver.name,
        "sha256": sha256_bytes(driver.read_bytes()),
        "version": version,
    }


def font_pack_identity(manifest: Path) -> tuple[Path, dict[str, object]]:
    resolved = manifest.resolve(strict=True)
    if not resolved.is_file() or resolved.is_symlink():
        raise GateError("font pack manifest is not a regular file")
    raw = resolved.read_bytes()
    if not raw or len(raw) > 256 * 1024:
        raise GateError("font pack manifest size is invalid")
    document = json.loads(raw)
    if not isinstance(document, dict) or document.get("schema") != "rxls.render-font-pack.v1":
        raise GateError("font pack schema differs")
    pack_sha256 = document.get("pack_sha256")
    if not isinstance(pack_sha256, str) or SHA256_RE.fullmatch(pack_sha256) is None:
        raise GateError("font pack identity is invalid")
    rows = document.get("fonts")
    if not isinstance(rows, list) or not 1 <= len(rows) <= 128:
        raise GateError("font pack face set is invalid")
    root = resolved.parent
    faces = []
    outputs: set[str] = set()
    for row in rows:
        if not isinstance(row, dict):
            raise GateError("font pack face row is invalid")
        output = row.get("output")
        digest = row.get("sha256")
        size = row.get("bytes")
        family = row.get("family")
        weight = row.get("weight")
        style = row.get("style")
        if (
            not isinstance(output, str)
            or not output.startswith("fonts/")
            or output.startswith("/")
            or ".." in Path(output).parts
            or output in outputs
            or not isinstance(digest, str)
            or SHA256_RE.fullmatch(digest) is None
            or not isinstance(size, int)
            or not 0 < size <= 128 * 1024 * 1024
            or not isinstance(family, str)
            or not family
            or not isinstance(weight, int)
            or not 1 <= weight <= 1000
            or style not in {"normal", "italic", "oblique"}
        ):
            raise GateError("font pack face row is invalid")
        outputs.add(output)
        path = root / output
        if not path.is_file() or path.is_symlink() or path.stat().st_size != size:
            raise GateError("font pack face file differs")
        if sha256_bytes(path.read_bytes()) != digest:
            raise GateError("font pack face hash differs")
        faces.append(
            {
                "family": family,
                "sha256": digest,
                "style": style,
                "weight": weight,
            }
        )
    return resolved, {
        "face_count": len(faces),
        "faces": faces,
        "manifest_sha256": sha256_bytes(raw),
        "pack_sha256": pack_sha256,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--driver",
        type=Path,
        default=Path("render/perf/target/release/rxls-render-perf"),
    )
    parser.add_argument("--case", action="append", choices=tuple(DEFAULT_BUDGETS))
    parser.add_argument("--repeat", type=int, default=2)
    parser.add_argument("--budget-file", type=Path)
    parser.add_argument(
        "--font-pack-manifest",
        type=Path,
        default=(
            Path(os.environ["RXLS_RENDER_FONT_PACK_MANIFEST"])
            if os.environ.get("RXLS_RENDER_FONT_PACK_MANIFEST")
            else None
        ),
    )
    parser.add_argument("--output", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.repeat < 2 or args.repeat > 10:
            raise GateError("--repeat must be between 2 and 10")
        budgets, budgets_sha256 = load_budgets(args.budget_file)
        identity = driver_identity(args.driver)
        selected = list(dict.fromkeys(args.case or DEFAULT_BUDGETS))
        font_manifest: Path | None = None
        font_identity: dict[str, object] | None = None
        if args.font_pack_manifest is not None:
            font_manifest, font_identity = font_pack_identity(args.font_pack_manifest)
        if FONT_PACK_CASES.intersection(selected) and font_manifest is None:
            raise GateError("selected shaped-text workloads require --font-pack-manifest")
        records = []
        passed = True
        for case in selected:
            samples = [
                run_sample(args.driver, case, budgets[case], font_manifest)
                for _ in range(args.repeat)
            ]
            record, case_passed = evaluate_case(case, budgets[case], samples)
            records.append(record)
            passed = passed and case_passed
        payload = {
            "budgets_sha256": budgets_sha256,
            "cases": records,
            "driver": identity,
            "font_pack": (
                {"configured": True, **font_identity}
                if font_identity is not None
                else {"configured": False}
            ),
            "passed": passed,
            "repeat": args.repeat,
            "schema": SCHEMA,
        }
        rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
        if args.output is None:
            sys.stdout.write(rendered)
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered, encoding="utf-8")
        return 0 if passed else 1
    except (GateError, json.JSONDecodeError, OSError, subprocess.SubprocessError) as error:
        print(f"render-performance-gates: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
