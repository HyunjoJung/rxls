#!/usr/bin/env python3
"""Fetch public spreadsheet corpora into gitignored local storage.

The downloaded files are intentionally placed under `local/` and are not
committed. This script commits only the recipe: source, license, path, and local
hashes in a manifest so parity runs can be reproduced.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DEST = ROOT / "local" / "public-corpus"
USER_AGENT = "rxls-public-corpus-fetcher"
SUPPORTED_EXTS = (".xls", ".xlsx", ".xlsm", ".xlsb", ".ods")

SOURCES = {
    "calamine": {
        "repo": "tafia/calamine",
        # Pinned from refs/heads/master on 2026-06-23 so another machine sees
        # the same corpus instead of whatever the branch contains later.
        "ref": "5d84fbf26de95324bd7d21b4aae77f649059bea1",
        "path": "tests",
        "license": "MIT",
        "source_url": "https://github.com/tafia/calamine/tree/master/tests",
    },
    "apache-poi": {
        "repo": "apache/poi",
        # Pinned from refs/heads/trunk on 2026-06-23.
        "ref": "aa268199243921dd0d9e1dc8d96cc06331280c94",
        "path": "test-data/spreadsheet",
        "license": "Apache-2.0",
        "source_url": "https://github.com/apache/poi/tree/trunk/test-data/spreadsheet",
    },
}


def request_json(url: str) -> object:
    req = Request(url, headers={"User-Agent": USER_AGENT})
    with urlopen(req, timeout=60) as resp:
        return json.load(resp)


def source_tree(repo: str, ref: str) -> list[dict]:
    url = f"https://api.github.com/repos/{repo}/git/trees/{quote(ref)}?recursive=1"
    data = request_json(url)
    if not isinstance(data, dict) or "tree" not in data:
        raise RuntimeError(f"unexpected GitHub tree response for {repo}@{ref}")
    return data["tree"]


def raw_url(repo: str, ref: str, path: str) -> str:
    return (
        "https://raw.githubusercontent.com/"
        f"{repo}/{quote(ref)}/"
        + "/".join(quote(part) for part in path.split("/"))
    )


def discover(source_name: str, max_bytes: int | None) -> list[dict]:
    source = SOURCES[source_name]
    repo = source["repo"]
    ref = source["ref"]
    root = source["path"].rstrip("/") + "/"
    files: list[dict] = []
    for item in source_tree(repo, ref):
        if item.get("type") != "blob":
            continue
        path = item.get("path", "")
        ext = Path(path).suffix.lower()
        size = int(item.get("size") or 0)
        if not path.startswith(root) or ext not in SUPPORTED_EXTS:
            continue
        if max_bytes is not None and size > max_bytes:
            continue
        files.append(
            {
                "source": source_name,
                "repo": repo,
                "ref": ref,
                "license": source["license"],
                "source_url": source["source_url"],
                "path": path,
                "size": size,
                "git_blob_sha": item.get("sha"),
                "url": raw_url(repo, ref, path),
            }
        )
    return sorted(files, key=lambda f: f["path"])


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def git_blob_sha(path: Path) -> str:
    h = hashlib.sha1()  # GitHub's pinned tree API exposes SHA-1 object IDs.
    h.update(f"blob {path.stat().st_size}\0".encode("ascii"))
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def matches_pinned_blob(file: dict, path: Path) -> bool:
    if path.stat().st_size != file["size"]:
        return False
    expected = file.get("git_blob_sha")
    return not expected or git_blob_sha(path) == expected


def resolve_dest(dest: Path) -> Path:
    return dest if dest.is_absolute() else ROOT / dest


def manifest_local_path(path: Path) -> str:
    try:
        return str(path.relative_to(ROOT)).replace(os.sep, "/")
    except ValueError:
        return str(path).replace(os.sep, "/")


def download_one(file: dict, dest: Path, force: bool) -> dict:
    dest = resolve_dest(dest)
    rel = Path(file["source"]) / file["path"]
    out = dest / rel
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists() and not force and matches_pinned_blob(file, out):
        file = dict(file)
        file["local_path"] = manifest_local_path(out)
        file["sha256"] = sha256(out)
        file["status"] = "cached"
        return file

    last_error: str | None = None
    for attempt in range(1, 4):
        try:
            req = Request(file["url"], headers={"User-Agent": USER_AGENT})
            with urlopen(req, timeout=90) as resp:
                payload = resp.read()
            if len(payload) != file["size"]:
                raise RuntimeError(
                    f"size mismatch: expected {file['size']}, got {len(payload)}"
                )
            out.write_bytes(payload)
            expected_blob = file.get("git_blob_sha")
            if expected_blob and git_blob_sha(out) != expected_blob:
                out.unlink(missing_ok=True)
                raise RuntimeError("Git blob hash mismatch")
            file = dict(file)
            file["local_path"] = manifest_local_path(out)
            file["sha256"] = sha256(out)
            file["status"] = "downloaded"
            return file
        except (HTTPError, URLError, RuntimeError) as exc:
            last_error = str(exc)
            time.sleep(0.5 * attempt)
    file = dict(file)
    file["status"] = "failed"
    file["error"] = last_error or "unknown error"
    return file


def write_manifest(dest: Path, files: list[dict]) -> None:
    dest = resolve_dest(dest)
    manifest = {
        "generated_by": "scripts/fetch-public-corpus.py",
        "formats": list(SUPPORTED_EXTS),
        "sources": SOURCES,
        "files": files,
    }
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def summarize(files: list[dict]) -> str:
    by_ext: dict[str, int] = {}
    by_source: dict[str, int] = {}
    bytes_total = 0
    for file in files:
        ext = Path(file["path"]).suffix.lower()
        by_ext[ext] = by_ext.get(ext, 0) + 1
        by_source[file["source"]] = by_source.get(file["source"], 0) + 1
        bytes_total += int(file.get("size") or 0)
    parts = [
        f"files={len(files)}",
        f"bytes={bytes_total}",
        "by_ext=" + ",".join(f"{k}:{by_ext[k]}" for k in sorted(by_ext)),
        "by_source=" + ",".join(f"{k}:{by_source[k]}" for k in sorted(by_source)),
    ]
    return " ".join(parts)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source",
        action="append",
        choices=sorted(SOURCES),
        help="source to fetch; repeatable; defaults to all",
    )
    parser.add_argument("--dest", type=Path, default=DEFAULT_DEST)
    parser.add_argument("--max-bytes", type=int, default=5_000_000)
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    selected = args.source or sorted(SOURCES)
    files: list[dict] = []
    for source_name in selected:
        discovered = discover(source_name, args.max_bytes)
        print(f"{source_name}: discovered {summarize(discovered)}")
        files.extend(discovered)
    files = sorted(files, key=lambda f: (f["source"], f["path"]))
    if args.limit is not None:
        files = files[: args.limit]

    if args.dry_run:
        print("dry-run:", summarize(files))
        return 0

    dest = resolve_dest(args.dest)
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        fetched = list(pool.map(lambda f: download_one(f, dest, args.force), files))
    write_manifest(dest, fetched)

    failed = [f for f in fetched if f.get("status") == "failed"]
    print("fetched:", summarize([f for f in fetched if f.get("status") != "failed"]))
    print(f"manifest: {manifest_local_path(dest / 'manifest.json')}")
    if failed:
        for file in failed[:20]:
            print(f"FAILED {file['source']} {file['path']}: {file.get('error')}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
