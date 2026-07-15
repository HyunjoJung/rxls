"""Shared helpers for source-pinned public-corpus oracle scripts."""

from __future__ import annotations

import glob
import hashlib
import importlib.metadata
import json
import os
import platform
from pathlib import Path
from typing import Iterable


READY_STATUSES = {"cached", "downloaded"}


def manifest_sha256(manifest_path: str | os.PathLike[str]) -> str:
    """Return the SHA-256 of the exact manifest bytes consumed by a run."""
    digest = hashlib.sha256()
    with open(manifest_path, "rb") as manifest:
        for chunk in iter(lambda: manifest.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def emit_parity_provenance(
    manifest_path: str | os.PathLike[str] | None,
    *,
    oracle_reader: str,
    package_distribution: str | None = None,
) -> None:
    """Print deterministic input identity and the installed oracle version."""
    if package_distribution is None:
        oracle_version = f"python-{platform.python_version()}"
    else:
        try:
            oracle_version = importlib.metadata.version(package_distribution)
        except importlib.metadata.PackageNotFoundError:
            oracle_version = "unavailable"
    if manifest_path and os.path.isfile(manifest_path):
        manifest_digest = manifest_sha256(manifest_path)
    elif manifest_path:
        manifest_digest = "unavailable"
    else:
        manifest_digest = "none"
    print(f"provenance: oracle_reader={oracle_reader} oracle_version={oracle_version}")
    print(f"provenance: input_manifest_sha256={manifest_digest}")


def resolve_binary(binary: str) -> str:
    """Resolve an rxls example binary, accepting Windows `.exe` suffixes."""
    path = os.path.abspath(binary)
    if not os.path.exists(path) and os.path.exists(path + ".exe"):
        return path + ".exe"
    return path


def report_source_root(
    manifest_path: str | os.PathLike[str] | None,
    corpus_path: str | os.PathLike[str] | None,
) -> str | None:
    """Return the source root used to make report paths machine-independent."""
    if manifest_path:
        return os.path.dirname(os.path.abspath(os.fspath(manifest_path)))
    if corpus_path:
        return os.path.abspath(os.fspath(corpus_path))
    return None


def report_path(
    path: str | os.PathLike[str], source_root: str | os.PathLike[str] | None = None
) -> str:
    """Render a path without serializing a checkout or runner home directory."""
    resolved = Path(path).resolve()
    for root, prefix in ((source_root, "corpus"), (Path.cwd().resolve(), None)):
        if root is None:
            continue
        try:
            relative = resolved.relative_to(Path(root).resolve())
        except ValueError:
            continue
        if prefix is not None:
            relative = Path(prefix) / relative
        return relative.as_posix()
    return resolved.name


def report_reason(
    reason: str,
    path: str | os.PathLike[str],
    source_root: str | os.PathLike[str] | None = None,
) -> str:
    """Replace a selected file's machine path if an oracle repeats it in an error."""
    replacement = report_path(path, source_root)
    raw = os.fspath(path)
    variants = {
        raw,
        os.path.abspath(raw),
        raw.replace("\\", "/"),
        raw.replace("/", "\\"),
        os.path.abspath(raw).replace("\\", "/"),
        os.path.abspath(raw).replace("/", "\\"),
    }
    for variant in sorted(variants, key=len, reverse=True):
        if variant:
            reason = reason.replace(variant, replacement)
    return reason


def entry_path(manifest_path: str | os.PathLike[str], entry: dict) -> str | None:
    """Return the local file path advertised by one manifest entry.

    `fetch-public-corpus.py` writes paths relative to the repository root, while
    small tests often write paths relative to the manifest directory. Accept both
    so the oracle scripts can be run from a repo checkout or a reduced fixture
    directory.
    """
    local_path = entry.get("local_path")
    if not local_path:
        return None
    if os.path.isabs(local_path):
        return local_path
    manifest_dir = os.path.dirname(os.path.abspath(os.fspath(manifest_path)))
    candidates = [
        os.path.abspath(local_path),
        os.path.abspath(os.path.join(manifest_dir, local_path)),
    ]
    for candidate in candidates:
        if os.path.exists(candidate):
            return candidate
    return candidates[0]


def manifest_files(
    manifest_path: str | os.PathLike[str],
    extensions: Iterable[str],
    limit: int | None = None,
) -> list[str]:
    """Select ready local files with one of `extensions` from a corpus manifest."""
    normalized_exts = {ext.lower() for ext in extensions}
    with open(manifest_path, encoding="utf-8") as fh:
        manifest = json.load(fh)
    entries = manifest.get("files", manifest) if isinstance(manifest, dict) else manifest
    files: list[str] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        status = entry.get("status")
        if status is not None and status not in READY_STATUSES:
            continue
        source_path = entry.get("path") or entry.get("local_path") or ""
        if Path(source_path).suffix.lower() not in normalized_exts:
            continue
        path = entry_path(manifest_path, entry)
        if path and os.path.exists(path):
            files.append(path)
    files.sort()
    if limit is not None:
        return files[:limit]
    return files


def corpus_files(
    corpus_path: str | os.PathLike[str],
    extensions: Iterable[str],
    limit: int | None = None,
) -> list[str]:
    """Select files with one of `extensions` from a flat corpus directory."""
    normalized_exts = {ext.lower() for ext in extensions}
    root = os.fspath(corpus_path)
    files: list[str] = []
    for ext in sorted(normalized_exts):
        suffix = ext if ext.startswith(".") else f".{ext}"
        files.extend(glob.glob(os.path.join(root, f"*{suffix}")))
    files.sort()
    if limit is not None:
        return files[:limit]
    return files
