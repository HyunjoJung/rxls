#!/usr/bin/env python3
"""Verify rxls VSIX structure, bundled renderer identity, and checksum."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import struct
import zipfile

MAX_ARCHIVE_BYTES = 16 * 1024 * 1024
MAX_UNPACKED_BYTES = 32 * 1024 * 1024
MAX_FILES = 96
REQUIRED = {
    "[Content_Types].xml",
    "extension.vsixmanifest",
    "extension/package.json",
    "extension/dist/extension.js",
    "extension/readme.md",
    "extension/changelog.md",
    "extension/LICENSE.txt",
    "extension/THIRD_PARTY_NOTICES.txt",
    "extension/media/icon.png",
    "extension/media/viewer/index.html",
    "extension/media/viewer/build-manifest.json",
    "extension/media/viewer/LICENSE.txt",
    "extension/media/viewer/THIRD_PARTY_NOTICES.txt",
    "extension/media/viewer/runtime/LICENSE",
    "extension/media/viewer/runtime/THIRD_PARTY_NOTICES.txt",
    "extension/media/viewer/runtime/js/client.mjs",
    "extension/media/viewer/runtime/js/worker.mjs",
    "extension/media/viewer/runtime/vscode-worker.js",
    "extension/media/viewer/runtime/pkg/rxls_render_wasm_bg.wasm",
}
FORBIDDEN_PARTS = {
    "node_modules",
    "src",
    "scripts",
    "test",
    "test-fixtures",
    ".env",
}


def sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def verify(
    vsix: pathlib.Path, renderer_root: pathlib.Path, checksum: pathlib.Path | None
) -> None:
    archive_bytes = vsix.read_bytes()
    if len(archive_bytes) > MAX_ARCHIVE_BYTES:
        raise ValueError("VSIX exceeds the 16 MiB archive limit")
    digest = sha256(archive_bytes)

    with zipfile.ZipFile(vsix, "r") as archive:
        infos = archive.infolist()
        names = [info.filename for info in infos]
        if names != sorted(names):
            raise ValueError("VSIX entries are not deterministically sorted")
        if len(names) != len(set(names)) or len(names) > MAX_FILES:
            raise ValueError("VSIX entry count is invalid")
        if sum(info.file_size for info in infos) > MAX_UNPACKED_BYTES:
            raise ValueError("VSIX exceeds the 32 MiB unpacked limit")
        missing = REQUIRED.difference(names)
        if missing:
            raise ValueError(f"VSIX is missing required files: {sorted(missing)}")
        for name in names:
            parts = pathlib.PurePosixPath(name).parts
            if name.startswith("/") or "\\" in name or ".." in parts:
                raise ValueError(f"unsafe VSIX path: {name}")
            if any(part in FORBIDDEN_PARTS for part in parts) or name.endswith(".map"):
                raise ValueError(f"forbidden VSIX entry: {name}")
            if infos[names.index(name)].date_time != (1980, 1, 1, 0, 0, 0):
                raise ValueError(f"non-deterministic timestamp: {name}")

        manifest = json.loads(archive.read("extension/package.json"))
        if (
            manifest.get("name") != "rxls-spreadsheet-preview"
            or manifest.get("publisher") != "HyunjoJung"
            or manifest.get("version") != "0.1.0"
        ):
            raise ValueError("packaged extension identity is invalid")
        if manifest.get("capabilities", {}).get("untrustedWorkspaces") != {
            "supported": True
        }:
            raise ValueError("untrusted-workspace capability is missing")

        icon = archive.read("extension/media/icon.png")
        if icon[:8] != b"\x89PNG\r\n\x1a\n" or len(icon) < 24:
            raise ValueError("extension icon is not PNG")
        width, height = struct.unpack(">II", icon[16:24])
        if width != height or not 128 <= width <= 512:
            raise ValueError("extension icon must be a 128-512 px square")

        build_manifest = json.loads(
            archive.read("extension/media/viewer/build-manifest.json")
        )
        if (
            build_manifest.get("schema") != "rxls.vscode-viewer.v1"
            or build_manifest.get("renderer", {}).get("version") != "0.2.0"
            or not str(build_manifest.get("renderer", {}).get("integrity", "")).startswith(
                "sha512-"
            )
        ):
            raise ValueError("viewer build manifest is invalid")

        bundled_worker = archive.read("extension/media/viewer/runtime/vscode-worker.js")
        worker_manifest = build_manifest.get("workerBundle", {})
        if (
            worker_manifest.get("path") != "runtime/vscode-worker.js"
            or worker_manifest.get("format") != "classic-single-file"
            or worker_manifest.get("sha256") != sha256(bundled_worker)
            or worker_manifest.get("bundler", {}).get("name") != "esbuild"
            or worker_manifest.get("bundler", {}).get("version") != "0.28.2"
            or not str(worker_manifest.get("bundler", {}).get("integrity", "")).startswith(
                "sha512-"
            )
            or b"rxls.vscode.worker.bootstrap.v1" not in bundled_worker
        ):
            raise ValueError("single-file VS Code worker identity is invalid")

        for relative in (
            "LICENSE",
            "THIRD_PARTY_NOTICES.txt",
            "package.json",
            "js/client.mjs",
            "js/protocol.mjs",
            "js/worker-runtime.mjs",
            "js/worker.mjs",
            "pkg/rxls_render_wasm_bg.wasm",
            "pkg/rxls_render_wasm.js",
        ):
            expected = (renderer_root / pathlib.PurePosixPath(relative)).read_bytes()
            actual = archive.read(f"extension/media/viewer/runtime/{relative}")
            if actual != expected:
                raise ValueError(f"renderer byte mismatch: {relative}")

    if checksum is not None:
        expected_line = f"{digest}  {vsix.name}\n"
        if checksum.read_text(encoding="ascii") != expected_line:
            raise ValueError("VSIX checksum file is stale")

    print(json.dumps({"files": len(names), "sha256": digest, "size": len(archive_bytes)}))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("vsix", type=pathlib.Path)
    parser.add_argument("--renderer-root", required=True, type=pathlib.Path)
    parser.add_argument("--checksum", type=pathlib.Path)
    args = parser.parse_args()
    verify(args.vsix.resolve(), args.renderer_root.resolve(), args.checksum)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
