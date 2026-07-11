"""Shared helpers for source-pinned public-corpus oracle scripts."""

from __future__ import annotations

import glob
import json
import os
from pathlib import Path
from typing import Iterable


READY_STATUSES = {"cached", "downloaded"}


def resolve_binary(binary: str) -> str:
    """Resolve an rxls example binary, accepting Windows `.exe` suffixes."""
    path = os.path.abspath(binary)
    if not os.path.exists(path) and os.path.exists(path + ".exe"):
        return path + ".exe"
    return path


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
