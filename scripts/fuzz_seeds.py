#!/usr/bin/env python3
"""Materialize, verify, and replay the deterministic fuzz seed manifest."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Callable, Sequence
from zipfile import ZIP_STORED, ZipFile, ZipInfo


SCHEMA = "rxls.fuzz-seeds.v1"
REPORT_SCHEMA = "rxls.fuzz-seed-manifest.v1"
REPLAY_SCHEMA = "rxls.fuzz-seed-replay.v1"
TARGETS = ("parse", "author", "edit", "formula")
SEED_ID_RE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
XLSX_CONTENT_TYPE = (
    b"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"
)
XLSM_CONTENT_TYPE = b"application/vnd.ms-excel.sheet.macroEnabled.main+xml"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _safe_name(value: object, label: str) -> str:
    if not isinstance(value, str) or not value or Path(value).name != value:
        raise ValueError(f"{label} must be a non-empty basename")
    return value


def _source_bytes(repo_root: Path, source: object) -> bytes:
    if not isinstance(source, str) or not source:
        raise ValueError("seed source must be a non-empty repository-relative path")
    root = repo_root.resolve()
    path = (root / source).resolve()
    try:
        path.relative_to(root)
    except ValueError as error:
        raise ValueError(f"seed source escapes repository: {source}") from error
    if not path.is_file():
        raise ValueError(f"seed source is not a file: {source}")
    return path.read_bytes()


def _xlsx_to_xlsm(data: bytes) -> bytes:
    """Derive one stable macro-enabled OPC fixture from the committed XLSX seed."""

    output = io.BytesIO()
    try:
        with ZipFile(io.BytesIO(data), "r") as source, ZipFile(
            output, "w", compression=ZIP_STORED
        ) as destination:
            names = source.namelist()
            if len(names) != len(set(names)):
                raise ValueError("XLSX seed has duplicate ZIP members")
            for name in sorted(names):
                payload = source.read(name)
                if name == "[Content_Types].xml":
                    if payload.count(XLSX_CONTENT_TYPE) != 1:
                        raise ValueError("XLSX seed has an unexpected workbook content type")
                    payload = payload.replace(XLSX_CONTENT_TYPE, XLSM_CONTENT_TYPE)
                info = ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = ZIP_STORED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                destination.writestr(info, payload)
    except (OSError, ValueError) as error:
        raise ValueError("could not derive deterministic XLSM seed") from error
    return output.getvalue()


def _seed_bytes(repo_root: Path, seed: dict[str, object]) -> bytes:
    encodings = [key for key in ("hex", "source") if key in seed]
    if len(encodings) != 1:
        raise ValueError("each seed must specify exactly one of hex or source")
    if encodings[0] == "source":
        data = _source_bytes(repo_root, seed["source"])
    else:
        encoded = seed["hex"]
        if not isinstance(encoded, str):
            raise ValueError("seed hex must be a string")
        try:
            data = bytes.fromhex(encoded)
        except ValueError as error:
            raise ValueError("seed hex is invalid") from error

    transform = seed.get("transform")
    if transform is None:
        return data
    if transform != "xlsx-to-xlsm" or encodings[0] != "source":
        raise ValueError(f"unsupported fuzz seed transform: {transform!r}")
    return _xlsx_to_xlsm(data)


def load_manifest(
    manifest: Path, repo_root: Path
) -> tuple[str, list[dict[str, object]]]:
    """Load and validate seeds, returning the source digest and normalized entries."""

    manifest_bytes = manifest.read_bytes()
    payload = json.loads(manifest_bytes)
    if not isinstance(payload, dict) or payload.get("schema") != SCHEMA:
        raise ValueError(f"unexpected fuzz seed manifest schema in {manifest}")
    seed_records = payload.get("seeds")
    if not isinstance(seed_records, list) or not seed_records:
        raise ValueError("fuzz seed manifest seeds must be a non-empty array")

    normalized_seeds: dict[str, dict[str, object]] = {}
    for seed in seed_records:
        if not isinstance(seed, dict):
            raise ValueError("fuzz seed definition must be an object")
        seed_id = seed.get("id")
        if not isinstance(seed_id, str) or SEED_ID_RE.fullmatch(seed_id) is None:
            raise ValueError(f"invalid fuzz seed id: {seed_id!r}")
        if seed_id in normalized_seeds:
            raise ValueError(f"repeated fuzz seed id: {seed_id}")
        name = _safe_name(seed.get("name"), f"{seed_id} seed name")
        expected_digest = seed.get("sha256")
        if (
            not isinstance(expected_digest, str)
            or len(expected_digest) != 64
            or any(character not in "0123456789abcdef" for character in expected_digest)
        ):
            raise ValueError(f"fuzz seed {seed_id} has an invalid sha256")
        data = _seed_bytes(repo_root, seed)
        actual_digest = _sha256(data)
        if actual_digest != expected_digest:
            raise ValueError(
                f"fuzz seed {seed_id} digest differs: "
                f"expected {expected_digest}, got {actual_digest}"
            )
        normalized_seeds[seed_id] = {
            "id": seed_id,
            "name": name,
            "bytes": len(data),
            "sha256": actual_digest,
            "data": data,
        }

    target_records = payload.get("targets")
    if not isinstance(target_records, list):
        raise ValueError("fuzz seed manifest targets must be an array")

    normalized_by_target: dict[str, dict[str, object]] = {}
    for target_record in target_records:
        if not isinstance(target_record, dict):
            raise ValueError("fuzz seed target record must be an object")
        target = target_record.get("target")
        if not isinstance(target, str) or target not in TARGETS:
            raise ValueError(f"unexpected fuzz target: {target!r}")
        if target in normalized_by_target:
            raise ValueError(f"repeated fuzz target: {target}")
        seeds = target_record.get("seeds")
        if not isinstance(seeds, list) or not seeds:
            raise ValueError(f"fuzz target {target} must have at least one seed")

        selected_seeds: list[dict[str, object]] = []
        names: set[str] = set()
        seed_ids: set[str] = set()
        for seed_id in seeds:
            if not isinstance(seed_id, str) or seed_id not in normalized_seeds:
                raise ValueError(f"fuzz target {target} has unknown seed {seed_id!r}")
            if seed_id in seed_ids:
                raise ValueError(f"fuzz target {target} repeats seed id {seed_id}")
            seed_ids.add(seed_id)
            seed = normalized_seeds[seed_id]
            name = str(seed["name"])
            if name in names:
                raise ValueError(f"fuzz target {target} repeats seed {name}")
            names.add(name)
            selected_seeds.append(seed)
        normalized_by_target[target] = {
            "target": target,
            "seeds": sorted(selected_seeds, key=lambda item: str(item["name"])),
        }

    if set(normalized_by_target) != set(TARGETS):
        missing = sorted(set(TARGETS) - set(normalized_by_target))
        raise ValueError(f"fuzz seed manifest is missing targets: {missing}")
    return _sha256(manifest_bytes), [normalized_by_target[target] for target in TARGETS]


def _public_records(records: list[dict[str, object]]) -> list[dict[str, object]]:
    return [
        {
            "target": record["target"],
            "seeds": [
                {key: seed[key] for key in ("name", "bytes", "sha256")}
                for seed in record["seeds"]  # type: ignore[index]
            ],
        }
        for record in records
    ]


def _write_json(path: Path, payload: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def materialize(
    manifest: Path, repo_root: Path, corpus_root: Path, report: Path
) -> dict[str, object]:
    """Create a clean deterministic corpus and write its evidence report."""

    source_digest, records = load_manifest(manifest, repo_root)
    if corpus_root.exists():
        shutil.rmtree(corpus_root)
    corpus_root.mkdir(parents=True)
    for record in records:
        target_root = corpus_root / str(record["target"])
        target_root.mkdir()
        for seed in record["seeds"]:  # type: ignore[index]
            (target_root / str(seed["name"])).write_bytes(seed["data"])  # type: ignore[index]

    payload: dict[str, object] = {
        "schema": REPORT_SCHEMA,
        "source_manifest": manifest.resolve().relative_to(repo_root.resolve()).as_posix(),
        "source_manifest_sha256": source_digest,
        "targets": _public_records(records),
    }
    _write_json(report, payload)
    return payload


def verify_materialized(
    manifest: Path, repo_root: Path, corpus_root: Path
) -> tuple[str, list[dict[str, object]]]:
    source_digest, records = load_manifest(manifest, repo_root)
    expected_files: set[Path] = set()
    for record in records:
        target = str(record["target"])
        for seed in record["seeds"]:  # type: ignore[index]
            path = corpus_root / target / str(seed["name"])  # type: ignore[index]
            expected_files.add(path)
            if not path.is_file():
                raise ValueError(f"materialized fuzz seed is missing: {path}")
            data = path.read_bytes()
            if len(data) != seed["bytes"] or _sha256(data) != seed["sha256"]:  # type: ignore[index]
                raise ValueError(f"materialized fuzz seed differs: {path}")
    actual_files = {path for path in corpus_root.glob("*/*") if path.is_file()}
    if actual_files != expected_files:
        raise ValueError(
            "materialized fuzz seed file set differs: "
            f"extra={sorted(str(path) for path in actual_files - expected_files)}"
        )
    return source_digest, records


def replay(
    manifest: Path,
    repo_root: Path,
    corpus_root: Path,
    report: Path,
    toolchain: str,
    cargo_fuzz_version: str,
    runner: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run,
) -> dict[str, object]:
    """Replay every seed once and write deterministic pass/fail evidence."""

    source_digest, records = verify_materialized(manifest, repo_root, corpus_root)
    results: list[dict[str, object]] = []
    passed = True
    for record in records:
        target = str(record["target"])
        seed_results: list[dict[str, object]] = []
        for seed in record["seeds"]:  # type: ignore[index]
            name = str(seed["name"])  # type: ignore[index]
            seed_path = corpus_root / target / name
            print(f"replay {target}/{name}", flush=True)
            completed = runner(
                [
                    "cargo",
                    f"+{toolchain}",
                    "fuzz",
                    "run",
                    target,
                    str(seed_path),
                    "--",
                    "-runs=1",
                    "-seed=1",
                    "-timeout=10",
                    "-rss_limit_mb=2048",
                ],
                cwd=repo_root,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            if completed.stdout:
                print(completed.stdout, end="" if completed.stdout.endswith("\n") else "\n")
            seed_passed = completed.returncode == 0
            passed = passed and seed_passed
            seed_results.append(
                {
                    "name": name,
                    "bytes": seed["bytes"],  # type: ignore[index]
                    "sha256": seed["sha256"],  # type: ignore[index]
                    "passed": seed_passed,
                }
            )
        results.append({"target": target, "seeds": seed_results})

    payload: dict[str, object] = {
        "schema": REPLAY_SCHEMA,
        "passed": passed,
        "source_manifest_sha256": source_digest,
        "toolchain": toolchain,
        "cargo_fuzz": cargo_fuzz_version,
        "libfuzzer_runs_per_seed": 1,
        "targets": results,
    }
    _write_json(report, payload)
    return payload


def _parser() -> argparse.ArgumentParser:
    repo_root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=repo_root)
    parser.add_argument(
        "--manifest", type=Path, default=repo_root / "fuzz" / "seeds" / "manifest.json"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    materialize_parser = subparsers.add_parser("materialize")
    materialize_parser.add_argument("--corpus-root", type=Path, required=True)
    materialize_parser.add_argument("--report", type=Path, required=True)

    replay_parser = subparsers.add_parser("replay")
    replay_parser.add_argument("--corpus-root", type=Path, required=True)
    replay_parser.add_argument("--report", type=Path, required=True)
    replay_parser.add_argument("--toolchain", required=True)
    replay_parser.add_argument("--cargo-fuzz-version", required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "materialize":
            materialize(args.manifest, args.repo_root, args.corpus_root, args.report)
            print(f"materialized deterministic fuzz seeds in {args.corpus_root}")
            return 0
        payload = replay(
            args.manifest,
            args.repo_root,
            args.corpus_root,
            args.report,
            args.toolchain,
            args.cargo_fuzz_version,
        )
        return 0 if payload["passed"] is True else 1
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"fuzz seed error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
