#!/usr/bin/env python3
"""Generate locked, path-neutral Cargo dependency evidence."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tomllib
import urllib.error
import urllib.request


CRATE_NAME = "rxls-render-wasm"
DEFAULT_MANIFEST = Path("bindings/render-wasm/Cargo.toml")
DEFAULT_NOTICE = Path("bindings/render-wasm/THIRD_PARTY_NOTICES.txt")
TARGET = "wasm32-unknown-unknown"
GENERATOR = "scripts/render_supply_chain.py"
NOTICE_TITLE = "RXLS RENDER WORKER THIRD-PARTY NOTICES"
PROFILE_CONFIGS = {
    "render-worker": {
        "crate_name": CRATE_NAME,
        "manifest": DEFAULT_MANIFEST,
        "notice_title": NOTICE_TITLE,
        "target": TARGET,
        "target_label": TARGET,
    },
    "core-wasm": {
        "crate_name": "rxls-wasm",
        "manifest": Path("bindings/wasm/Cargo.toml"),
        "notice_title": "RXLS WASM THIRD-PARTY NOTICES",
        "target": TARGET,
        "target_label": TARGET,
    },
    "mcp": {
        "crate_name": "rxls-mcp",
        "manifest": Path("bindings/mcp/Cargo.toml"),
        "notice_title": "RXLS MCP THIRD-PARTY NOTICES",
        "target": (
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ),
        "target_label": "Linux, macOS, and Windows (x86_64/aarch64)",
    },
}
LEGAL_FILE_PREFIXES = (
    "license",
    "licence",
    "copying",
    "notice",
    "copyright",
    "unlicense",
)
MAX_LEGAL_FILE_BYTES = 512 * 1024
MAX_NOTICE_BYTES = 512 * 1024
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
LEGAL_URL_OVERRIDES = {
    # rmcp 3.1.4 declares Apache-2.0 but its crates.io archive omits the
    # workspace-root legal file. Pin the exact upstream release commit and
    # content hash so a registry packaging omission cannot drop the notice.
    ("rmcp", "3.1.4"): {
        "name": "UPSTREAM-LICENSE-rust-sdk-4a738b9d",
        "url": (
            "https://raw.githubusercontent.com/modelcontextprotocol/rust-sdk/"
            "4a738b9dd99eaca418b614afa433a0cbdaf8d056/LICENSE"
        ),
        "sha256": "0382b0057770ca05e9c350a50aa3b1c1fea84da0bc81d723bf00b9aa841be58a",
    },
    ("rmcp-macros", "3.1.4"): {
        "name": "UPSTREAM-LICENSE-rust-sdk-4a738b9d",
        "url": (
            "https://raw.githubusercontent.com/modelcontextprotocol/rust-sdk/"
            "4a738b9dd99eaca418b614afa433a0cbdaf8d056/LICENSE"
        ),
        "sha256": "0382b0057770ca05e9c350a50aa3b1c1fea84da0bc81d723bf00b9aa841be58a",
    },
}


class SupplyChainError(ValueError):
    """Raised when locked dependency evidence cannot be produced safely."""


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _cargo_metadata_one(manifest_path: Path, target: str | None) -> dict[str, object]:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--manifest-path",
        str(manifest_path),
    ]
    if target is not None:
        command[4:4] = ["--filter-platform", target]
    completed = subprocess.run(
        command,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    document = json.loads(completed.stdout)
    if not isinstance(document, dict):
        raise SupplyChainError("cargo metadata must contain an object")
    return document


def _merge_metadata(documents: list[dict[str, object]]) -> dict[str, object]:
    if not documents:
        raise SupplyChainError("at least one cargo metadata document is required")
    packages: dict[str, dict[str, object]] = {}
    workspace_members: set[str] = set()
    node_dependencies: dict[str, dict[str, dict[str, object]]] = {}
    for document in documents:
        for package in document.get("packages", []):
            packages[str(package["id"])] = package
        workspace_members.update(str(item) for item in document.get("workspace_members", []))
        resolve = document.get("resolve")
        if not isinstance(resolve, dict):
            raise SupplyChainError("cargo metadata is missing its resolved dependency graph")
        for node in resolve.get("nodes", []):
            package_id = str(node["id"])
            dependencies = node_dependencies.setdefault(package_id, {})
            for dependency in node.get("deps", []):
                dependency_id = str(dependency["pkg"])
                existing = dependencies.get(dependency_id)
                if existing is None:
                    dependencies[dependency_id] = dependency
                    continue
                known = {
                    json.dumps(kind, sort_keys=True)
                    for kind in existing.get("dep_kinds", [])
                }
                for kind in dependency.get("dep_kinds", []):
                    encoded = json.dumps(kind, sort_keys=True)
                    if encoded not in known:
                        existing.setdefault("dep_kinds", []).append(kind)
                        known.add(encoded)
    nodes = [
        {"id": package_id, "deps": list(sorted(dependencies.values(), key=lambda item: str(item["pkg"])))}
        for package_id, dependencies in sorted(node_dependencies.items())
    ]
    return {
        "packages": list(sorted(packages.values(), key=lambda item: str(item["id"]))),
        "workspace_members": sorted(workspace_members),
        "resolve": {"nodes": nodes},
    }


def cargo_metadata(
    manifest_path: Path,
    *,
    target: str | tuple[str, ...] | None = TARGET,
) -> dict[str, object]:
    if isinstance(target, tuple):
        return _merge_metadata(
            [_cargo_metadata_one(manifest_path, item) for item in target]
        )
    return _cargo_metadata_one(manifest_path, target)


def cargo_lock(manifest_path: Path) -> tuple[dict[str, object], str]:
    lock_path = manifest_path.parent / "Cargo.lock"
    payload = lock_path.read_bytes()
    document = tomllib.loads(payload.decode("utf-8"))
    if document.get("version") != 4 or not isinstance(document.get("package"), list):
        raise SupplyChainError("render-WASM Cargo.lock must use lock format 4")
    return document, sha256_bytes(payload)


def _normal_dependencies(node: dict[str, object]) -> list[str]:
    result: list[str] = []
    for dependency in node.get("deps", []):
        dep_kinds = dependency.get("dep_kinds", [])
        if any(kind.get("kind") is None for kind in dep_kinds):
            result.append(str(dependency["pkg"]))
    return sorted(set(result))


def production_closure(
    metadata: dict[str, object],
    *,
    crate_name: str = CRATE_NAME,
) -> tuple[str, dict[str, dict[str, object]], dict[str, list[str]]]:
    packages = {
        str(package["id"]): package for package in metadata.get("packages", [])
    }
    workspace_members = [str(item) for item in metadata.get("workspace_members", [])]
    roots = [
        item
        for item in workspace_members
        if packages.get(item, {}).get("name") == crate_name
    ]
    if len(roots) != 1:
        raise SupplyChainError(f"expected exactly one {crate_name} workspace root")
    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict):
        raise SupplyChainError("cargo metadata is missing its resolved dependency graph")
    nodes = {str(node["id"]): node for node in resolve.get("nodes", [])}
    root_id = roots[0]
    closure: set[str] = set()
    adjacency: dict[str, list[str]] = {}
    pending = [root_id]
    while pending:
        package_id = pending.pop()
        if package_id in closure:
            continue
        if package_id not in packages or package_id not in nodes:
            raise SupplyChainError("resolved dependency graph references an unknown package")
        closure.add(package_id)
        children = _normal_dependencies(nodes[package_id])
        adjacency[package_id] = children
        pending.extend(children)
    selected = {package_id: packages[package_id] for package_id in closure}
    return root_id, selected, adjacency


def _lock_index(lock: dict[str, object]) -> dict[tuple[str, str, str | None], dict]:
    index: dict[tuple[str, str, str | None], dict] = {}
    for package in lock.get("package", []):
        key = (
            str(package["name"]),
            str(package["version"]),
            str(package["source"]) if package.get("source") is not None else None,
        )
        if key in index:
            raise SupplyChainError("Cargo.lock contains a duplicate package identity")
        index[key] = package
    return index


def _package_lock_entry(package: dict[str, object], index: dict) -> dict:
    key = (
        str(package["name"]),
        str(package["version"]),
        str(package["source"]) if package.get("source") is not None else None,
    )
    entry = index.get(key)
    if entry is None:
        raise SupplyChainError(
            f"Cargo.lock is missing {package['name']} {package['version']}"
        )
    return entry


def _package_identity(package: dict[str, object]) -> str:
    return f"{package['name']} {package['version']}"


def _registry_archive_candidates(package: dict[str, object]) -> list[Path]:
    archive_name = f"{package['name']}-{package['version']}.crate"
    manifest = Path(str(package["manifest_path"]))
    if not manifest.is_absolute():
        manifest = Path(os.path.abspath(manifest))
    package_root = manifest.parent
    candidates: list[Path] = []

    registry_id = package_root.parent
    registry_src = registry_id.parent
    registry_root = registry_src.parent
    if registry_src.name == "src" and registry_root.name == "registry":
        candidates.append(
            registry_root / "cache" / registry_id.name / archive_name
        )

    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    cache_root = cargo_home / "registry" / "cache"
    if cache_root.is_dir():
        candidates.extend(cache_root.glob(f"*/{archive_name}"))

    unique: dict[str, Path] = {}
    for candidate in candidates:
        unique.setdefault(os.path.abspath(candidate), candidate)
    return [unique[key] for key in sorted(unique)]


def _verified_registry_archive(
    package: dict[str, object], expected_checksum: str
) -> bytes:
    identity = _package_identity(package)
    source = package.get("source")
    if not isinstance(source, str) or not source.startswith("registry+"):
        raise SupplyChainError(f"{identity} is not a Cargo registry dependency")
    if SHA256_RE.fullmatch(expected_checksum) is None:
        raise SupplyChainError(f"{identity} lacks a locked registry checksum")

    found = False
    unreadable = False
    for candidate in _registry_archive_candidates(package):
        if not candidate.is_file():
            continue
        found = True
        try:
            payload = candidate.read_bytes()
        except OSError:
            unreadable = True
            continue
        if sha256_bytes(payload) == expected_checksum:
            return payload
    if unreadable:
        raise SupplyChainError(f"{identity} registry archive cannot be read")
    if found:
        raise SupplyChainError(
            f"{identity} registry archive checksum differs from Cargo.lock"
        )
    raise SupplyChainError(f"{identity} registry archive is missing")


def _declared_license_path(package: dict[str, object]) -> str | None:
    declared = package.get("license_file")
    if not declared:
        return None
    manifest = Path(str(package["manifest_path"]))
    if not manifest.is_absolute():
        manifest = Path(os.path.abspath(manifest))
    package_root = manifest.parent
    candidate = Path(str(declared))
    if candidate.is_absolute():
        try:
            candidate = candidate.relative_to(package_root)
        except ValueError as error:
            raise SupplyChainError("crate license_file escapes its package root") from error
    parts = candidate.parts
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise SupplyChainError("crate license_file is not a safe package path")
    return "/".join(parts)


def _legal_url_override(package: dict[str, object]) -> tuple[str, bytes] | None:
    identity = (str(package["name"]), str(package["version"]))
    override = LEGAL_URL_OVERRIDES.get(identity)
    if override is None:
        return None
    request = urllib.request.Request(
        str(override["url"]),
        headers={"User-Agent": "rxls-supply-chain-audit"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = response.read(MAX_LEGAL_FILE_BYTES + 1)
    if not payload or len(payload) > MAX_LEGAL_FILE_BYTES:
        raise SupplyChainError(f"{_package_identity(package)} legal override is invalid")
    if sha256_bytes(payload) != override["sha256"]:
        raise SupplyChainError(
            f"{_package_identity(package)} legal override differs from its pinned hash"
        )
    try:
        payload.decode("utf-8")
    except UnicodeDecodeError as error:
        raise SupplyChainError("legal override must be UTF-8") from error
    return str(override["name"]), payload


def _legal_files(
    package: dict[str, object], expected_checksum: str
) -> list[tuple[str, bytes]]:
    payload = _verified_registry_archive(package, expected_checksum)
    identity = _package_identity(package)
    archive_root = f"{package['name']}-{package['version']}"
    declared = _declared_license_path(package)
    candidates: dict[str, tarfile.TarInfo] = {}
    try:
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            for member in archive.getmembers():
                raw_name = (
                    member.name[:-1]
                    if member.isdir() and member.name.endswith("/")
                    else member.name
                )
                if (
                    not raw_name
                    or raw_name.startswith("/")
                    or "\\" in raw_name
                    or any(part in ("", ".", "..") for part in raw_name.split("/"))
                ):
                    raise SupplyChainError(f"{identity} has an unsafe archive member")
                parts = raw_name.split("/")
                if parts[0] != archive_root:
                    raise SupplyChainError(
                        f"{identity} archive member escapes its package root"
                    )
                relative = "/".join(parts[1:])
                if not relative:
                    continue
                is_root_legal = (
                    "/" not in relative
                    and relative.lower().startswith(LEGAL_FILE_PREFIXES)
                )
                if not is_root_legal and relative != declared:
                    continue
                if not member.isfile():
                    raise SupplyChainError(
                        f"{identity} legal file is not a regular archive member"
                    )
                if relative in candidates:
                    raise SupplyChainError(
                        f"{identity} archive contains a duplicate legal file"
                    )
                candidates[relative] = member

            if not candidates:
                override = _legal_url_override(package)
                if override is not None:
                    return [override]
                raise SupplyChainError(
                    f"{identity} has no distributable legal file in its registry archive"
                )
            result: list[tuple[str, bytes]] = []
            for name, member in sorted(candidates.items()):
                if member.size <= 0 or member.size > MAX_LEGAL_FILE_BYTES:
                    raise SupplyChainError(f"{identity} has an invalid legal file")
                stream = archive.extractfile(member)
                if stream is None:
                    raise SupplyChainError(f"{identity} legal file cannot be read")
                legal_payload = stream.read(MAX_LEGAL_FILE_BYTES + 1)
                if len(legal_payload) != member.size:
                    raise SupplyChainError(f"{identity} has an invalid legal file")
                try:
                    legal_payload.decode("utf-8")
                except UnicodeDecodeError as error:
                    raise SupplyChainError("crate legal files must be UTF-8") from error
                result.append((name, legal_payload))
            return result
    except SupplyChainError:
        raise
    except (EOFError, OSError, tarfile.TarError) as error:
        raise SupplyChainError(f"{identity} registry archive is invalid") from error


def _package_sort_key(package: dict[str, object]) -> tuple[str, str, str]:
    return (
        str(package["name"]),
        str(package["version"]),
        str(package.get("source") or ""),
    )


def render_notice(
    metadata: dict[str, object],
    lock: dict[str, object],
    lock_sha256: str,
    *,
    crate_name: str = CRATE_NAME,
    manifest_label: Path = DEFAULT_MANIFEST,
    notice_title: str = NOTICE_TITLE,
    target_label: str = TARGET,
) -> tuple[str, dict[str, int]]:
    _, closure, _ = production_closure(metadata, crate_name=crate_name)
    third_party = sorted(
        (package for package in closure.values() if package.get("source") is not None),
        key=_package_sort_key,
    )
    if not third_party:
        raise SupplyChainError(f"{crate_name} production closure has no third-party packages")
    index = _lock_index(lock)
    package_legal_files: dict[tuple[str, str], list[tuple[str, str]]] = {}
    legal_payloads: dict[str, bytes] = {}
    legal_references: dict[str, list[str]] = {}
    package_checksums: dict[tuple[str, str], str] = {}
    for package in third_party:
        identity = (str(package["name"]), str(package["version"]))
        entry = _package_lock_entry(package, index)
        checksum = entry.get("checksum")
        if not isinstance(checksum, str) or SHA256_RE.fullmatch(checksum) is None:
            raise SupplyChainError(
                f"{package['name']} {package['version']} lacks a locked registry checksum"
            )
        records: list[tuple[str, str]] = []
        for filename, payload in _legal_files(package, checksum):
            digest = sha256_bytes(payload)
            legal_payloads.setdefault(digest, payload)
            reference = f"{identity[0]} {identity[1]}/{filename}"
            legal_references.setdefault(digest, []).append(reference)
            records.append((filename, digest))
        package_legal_files[identity] = records
        package_checksums[identity] = checksum

    separator = "=" * 79
    if target_label == TARGET:
        notice_scope = [
            "The npm package has no npm runtime dependencies. This notice conservatively",
            "covers every third-party crate reachable through normal Cargo edges used to",
            "produce the WebAssembly artifact, including proc-macro support. Legal-file",
            "text is identified and deduplicated by raw SHA-256. Embedded legal text is",
            "normalized from CRLF or CR to LF for deterministic display; framing and",
        ]
    else:
        notice_scope = [
            "This notice conservatively covers every third-party crate reachable through",
            "normal Cargo edges for supported native targets, including proc-macro support.",
            "Legal-file text is identified and deduplicated by raw SHA-256. Embedded legal",
            "text is normalized from CRLF or CR to LF for deterministic display; framing and",
        ]
    lines = [
        notice_title,
        f"Generated by {GENERATOR}. Do not edit manually.",
        "",
        "Scope:",
        f"- Manifest: {manifest_label.as_posix()}",
        f"- Target: {target_label}",
        "- Dependency edges: Cargo normal edges for the production target",
        f"- Cargo lock SHA-256: {lock_sha256}",
        f"- Third-party packages: {len(third_party)}",
        f"- Unique legal texts: {len(legal_payloads)}",
        "",
        *notice_scope,
        "line-break normalization are not part of the referenced legal-file bytes.",
        "",
    ]
    for package in third_party:
        license_expression = package.get("license")
        if not isinstance(license_expression, str) or not license_expression.strip():
            raise SupplyChainError(
                f"{package['name']} {package['version']} lacks a license expression"
            )
        identity = (str(package["name"]), str(package["version"]))
        checksum = package_checksums[identity]
        lines.extend(
            [
                separator,
                f"PACKAGE: {identity[0]} {identity[1]}",
                f"Cargo source: {package['source']}",
                f"Declared license expression: {license_expression}",
                f"Registry archive SHA-256: {checksum}",
                "Legal files:",
            ]
        )
        override = LEGAL_URL_OVERRIDES.get(identity)
        if override is not None:
            lines.append(f"Pinned legal source: {override['url']}")
        for filename, digest in package_legal_files[identity]:
            lines.append(f"- {filename}: {digest}")
        lines.append("")

    lines.extend([separator, "DEDUPLICATED LEGAL TEXTS", ""])
    for digest in sorted(legal_payloads):
        payload = legal_payloads[digest]
        references = sorted(set(legal_references[digest]))
        lines.extend(
            [
                separator,
                f"LEGAL TEXT SHA-256: {digest}",
                "Referenced by:",
                *(f"- {reference}" for reference in references),
                "----- BEGIN LEGAL TEXT -----",
            ]
        )
        text = payload.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
        lines.append(text.rstrip("\n"))
        lines.extend(["----- END LEGAL TEXT -----", ""])
    rendered = "\n".join(lines).rstrip() + "\n"
    if len(rendered.encode("utf-8")) > MAX_NOTICE_BYTES:
        raise SupplyChainError("third-party notice exceeds its deterministic size budget")
    if "file://" in rendered or re.search(r"/(?:Users|home)/[^/\s]+/", rendered):
        raise SupplyChainError("third-party notice contains a host path")
    return rendered, {
        "packages": len(third_party),
        "legal_texts": len(legal_payloads),
    }


def package_ref(package: dict[str, object]) -> str:
    return f"pkg:cargo/{package['name']}@{package['version']}"


def _spdx_expression(package: dict[str, object]) -> str | None:
    expression = package.get("license")
    if not expression:
        return None
    # Older Cargo manifests used `/` for a dual-license choice. CycloneDX's
    # expression field requires current SPDX syntax, where that choice is OR.
    return re.sub(r"\s*/\s*", " OR ", str(expression))


def _component(package: dict[str, object], lock_index: dict) -> dict[str, object]:
    reference = package_ref(package)
    component: dict[str, object] = {
        "type": "library",
        "bom-ref": reference,
        "name": str(package["name"]),
        "version": str(package["version"]),
        "purl": reference,
        "scope": "required",
    }
    license_expression = _spdx_expression(package)
    if license_expression:
        component["licenses"] = [{"expression": str(license_expression)}]
    source = package.get("source")
    if source is not None:
        entry = _package_lock_entry(package, lock_index)
        checksum = entry.get("checksum")
        if not isinstance(checksum, str) or SHA256_RE.fullmatch(checksum) is None:
            raise SupplyChainError("registry dependency lacks a locked SHA-256 checksum")
        component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        component["properties"] = [
            {"name": "cargo:source", "value": str(source)}
        ]
    return component


def make_sbom(
    metadata: dict[str, object],
    lock: dict[str, object],
    lock_sha256: str,
    *,
    crate_name: str = CRATE_NAME,
    target_label: str = TARGET,
) -> dict[str, object]:
    root_id, closure, adjacency = production_closure(metadata, crate_name=crate_name)
    lock_index = _lock_index(lock)
    root = closure[root_id]
    references = {package_ref(package) for package in closure.values()}
    if len(references) != len(closure):
        raise SupplyChainError("production closure contains ambiguous Cargo package refs")
    components = [
        _component(package, lock_index)
        for package_id, package in closure.items()
        if package_id != root_id
    ]
    components.sort(key=lambda item: str(item["bom-ref"]))
    dependencies = []
    for package_id, package in closure.items():
        child_refs = sorted(
            package_ref(closure[child])
            for child in adjacency[package_id]
            if child in closure
        )
        dependencies.append(
            {"ref": package_ref(package), "dependsOn": child_refs}
        )
    dependencies.sort(key=lambda item: str(item["ref"]))
    return {
        "$schema": "http://cyclonedx.org/schema/bom-1.5.schema.json",
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": _component(root, lock_index),
            "properties": [
                {"name": "rxls:cargo-lock-sha256", "value": lock_sha256},
                {"name": "rxls:dependency-edges", "value": "normal"},
                {"name": "rxls:generator", "value": GENERATOR},
                {"name": "rxls:locked", "value": "true"},
                {"name": "rxls:target", "value": target_label},
            ],
        },
        "components": components,
        "dependencies": dependencies,
    }


def render_sbom(
    metadata: dict[str, object],
    lock: dict[str, object],
    lock_sha256: str,
    *,
    crate_name: str = CRATE_NAME,
    target_label: str = TARGET,
) -> tuple[str, dict[str, int]]:
    document = make_sbom(
        metadata,
        lock,
        lock_sha256,
        crate_name=crate_name,
        target_label=target_label,
    )
    rendered = json.dumps(document, indent=2, sort_keys=True) + "\n"
    if "file://" in rendered or re.search(r"/(?:Users|home)/[^/\s]+/", rendered):
        raise SupplyChainError("CycloneDX evidence contains a host path")
    return rendered, {
        "components": len(document["components"]),
        "dependency_nodes": len(document["dependencies"]),
    }


def _inputs(
    manifest_path: Path,
    *,
    target: str | tuple[str, ...] | None = TARGET,
) -> tuple[dict[str, object], dict[str, object], str]:
    metadata = cargo_metadata(manifest_path, target=target)
    lock, lock_sha256 = cargo_lock(manifest_path)
    return metadata, lock, lock_sha256


def _write_or_check(
    rendered: str, *, output: Path | None, check: Path | None, label: str
) -> None:
    if output is not None:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(rendered.encode("utf-8"))
        return
    if check is None:
        raise SupplyChainError(f"{label} requires --output or --check")
    try:
        existing = check.read_bytes().decode("utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise SupplyChainError(f"cannot read checked {label}") from error
    if existing != rendered:
        raise SupplyChainError(f"checked {label} differs from the locked closure")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for command in ("notice", "sbom"):
        child = subparsers.add_parser(command)
        child.add_argument(
            "--profile",
            choices=sorted(PROFILE_CONFIGS),
            default="render-worker",
        )
        child.add_argument("--manifest-path", type=Path)
        destination = child.add_mutually_exclusive_group(required=True)
        destination.add_argument("--output", type=Path)
        destination.add_argument("--check", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        profile = PROFILE_CONFIGS[args.profile]
        default_manifest = Path(profile["manifest"])
        manifest_path = args.manifest_path or default_manifest
        target = profile["target"]
        metadata, lock, lock_sha256 = _inputs(
            manifest_path,
            target=target if isinstance(target, (str, tuple)) else None,
        )
        if args.command == "notice":
            rendered, summary = render_notice(
                metadata,
                lock,
                lock_sha256,
                crate_name=str(profile["crate_name"]),
                manifest_label=default_manifest,
                notice_title=str(profile["notice_title"]),
                target_label=str(profile["target_label"]),
            )
        else:
            rendered, summary = render_sbom(
                metadata,
                lock,
                lock_sha256,
                crate_name=str(profile["crate_name"]),
                target_label=str(profile["target_label"]),
            )
        _write_or_check(
            rendered,
            output=args.output,
            check=args.check,
            label=args.command,
        )
        mode = "generated" if args.output is not None else "verified"
        details = " ".join(f"{name}={value}" for name, value in sorted(summary.items()))
        print(f"render supply chain: {args.command} {mode} {details}")
    except (
        OSError,
        UnicodeDecodeError,
        ValueError,
        subprocess.CalledProcessError,
        urllib.error.URLError,
        json.JSONDecodeError,
        tomllib.TOMLDecodeError,
    ) as error:
        print(f"render supply chain: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
