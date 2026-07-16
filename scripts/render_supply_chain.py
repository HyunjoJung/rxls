#!/usr/bin/env python3
"""Generate locked, path-neutral render-worker dependency evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Callable


CRATE_NAME = "rxls-render-wasm"
DEFAULT_MANIFEST = Path("bindings/render-wasm/Cargo.toml")
DEFAULT_NOTICE = Path("bindings/render-wasm/THIRD_PARTY_NOTICES.txt")
TARGET = "wasm32-unknown-unknown"
GENERATOR = "scripts/render_supply_chain.py"
NOTICE_TITLE = "RXLS RENDER WORKER THIRD-PARTY NOTICES"
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


class SupplyChainError(ValueError):
    """Raised when locked dependency evidence cannot be produced safely."""


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def cargo_metadata(manifest_path: Path) -> dict[str, object]:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--filter-platform",
        TARGET,
        "--manifest-path",
        str(manifest_path),
    ]
    completed = subprocess.run(command, check=True, capture_output=True, text=True)
    document = json.loads(completed.stdout)
    if not isinstance(document, dict):
        raise SupplyChainError("cargo metadata must contain an object")
    return document


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
) -> tuple[str, dict[str, dict[str, object]], dict[str, list[str]]]:
    packages = {
        str(package["id"]): package for package in metadata.get("packages", [])
    }
    workspace_members = [str(item) for item in metadata.get("workspace_members", [])]
    roots = [
        item
        for item in workspace_members
        if packages.get(item, {}).get("name") == CRATE_NAME
    ]
    if len(roots) != 1:
        raise SupplyChainError(f"expected exactly one {CRATE_NAME} workspace root")
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


def _legal_files(package: dict[str, object]) -> list[tuple[str, bytes]]:
    package_root = Path(str(package["manifest_path"])).resolve().parent
    candidates: dict[str, Path] = {}
    for candidate in package_root.iterdir():
        lowered = candidate.name.lower()
        if candidate.is_file() and lowered.startswith(LEGAL_FILE_PREFIXES):
            candidates[candidate.name] = candidate
    declared = package.get("license_file")
    if declared:
        candidate = Path(str(declared)).resolve()
        try:
            relative = candidate.relative_to(package_root).as_posix()
        except ValueError as error:
            raise SupplyChainError("crate license_file escapes its package root") from error
        if not candidate.is_file():
            raise SupplyChainError("crate license_file is missing")
        candidates[relative] = candidate
    if not candidates:
        raise SupplyChainError(
            f"{package['name']} {package['version']} has no distributable legal file"
        )
    result: list[tuple[str, bytes]] = []
    for name, path in sorted(candidates.items()):
        payload = path.read_bytes()
        if not payload or len(payload) > MAX_LEGAL_FILE_BYTES:
            raise SupplyChainError(
                f"{package['name']} {package['version']} has an invalid legal file"
            )
        try:
            payload.decode("utf-8")
        except UnicodeDecodeError as error:
            raise SupplyChainError("crate legal files must be UTF-8") from error
        result.append((name, payload))
    return result


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
    legal_file_loader: Callable[[dict[str, object]], list[tuple[str, bytes]]] = _legal_files,
) -> tuple[str, dict[str, int]]:
    _, closure, _ = production_closure(metadata)
    third_party = sorted(
        (package for package in closure.values() if package.get("source") is not None),
        key=_package_sort_key,
    )
    if not third_party:
        raise SupplyChainError("render-WASM production closure has no third-party packages")
    index = _lock_index(lock)
    package_legal_files: dict[tuple[str, str], list[tuple[str, str]]] = {}
    legal_payloads: dict[str, bytes] = {}
    legal_references: dict[str, list[str]] = {}
    for package in third_party:
        identity = (str(package["name"]), str(package["version"]))
        records: list[tuple[str, str]] = []
        for filename, payload in legal_file_loader(package):
            digest = sha256_bytes(payload)
            legal_payloads.setdefault(digest, payload)
            reference = f"{identity[0]} {identity[1]}/{filename}"
            legal_references.setdefault(digest, []).append(reference)
            records.append((filename, digest))
        package_legal_files[identity] = records

    separator = "=" * 79
    lines = [
        NOTICE_TITLE,
        f"Generated by {GENERATOR}. Do not edit manually.",
        "",
        "Scope:",
        f"- Manifest: {DEFAULT_MANIFEST.as_posix()}",
        f"- Target: {TARGET}",
        "- Dependency edges: Cargo normal edges for the production target",
        f"- Cargo lock SHA-256: {lock_sha256}",
        f"- Third-party packages: {len(third_party)}",
        f"- Unique legal texts: {len(legal_payloads)}",
        "",
        "The npm package has no npm runtime dependencies. This notice conservatively",
        "covers every third-party crate reachable through normal Cargo edges used to",
        "produce the WebAssembly artifact, including proc-macro support. Legal-file",
        "text is reproduced and deduplicated by raw SHA-256; framing and terminal",
        "line-break normalization are not part of the referenced legal-file bytes.",
        "",
    ]
    for package in third_party:
        entry = _package_lock_entry(package, index)
        checksum = entry.get("checksum")
        if not isinstance(checksum, str) or SHA256_RE.fullmatch(checksum) is None:
            raise SupplyChainError(
                f"{package['name']} {package['version']} lacks a locked registry checksum"
            )
        license_expression = package.get("license")
        if not isinstance(license_expression, str) or not license_expression.strip():
            raise SupplyChainError(
                f"{package['name']} {package['version']} lacks a license expression"
            )
        identity = (str(package["name"]), str(package["version"]))
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
        text = payload.decode("utf-8")
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
    metadata: dict[str, object], lock: dict[str, object], lock_sha256: str
) -> dict[str, object]:
    root_id, closure, adjacency = production_closure(metadata)
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
                {"name": "rxls:target", "value": TARGET},
            ],
        },
        "components": components,
        "dependencies": dependencies,
    }


def render_sbom(
    metadata: dict[str, object], lock: dict[str, object], lock_sha256: str
) -> tuple[str, dict[str, int]]:
    document = make_sbom(metadata, lock, lock_sha256)
    rendered = json.dumps(document, indent=2, sort_keys=True) + "\n"
    if "file://" in rendered or re.search(r"/(?:Users|home)/[^/\s]+/", rendered):
        raise SupplyChainError("CycloneDX evidence contains a host path")
    return rendered, {
        "components": len(document["components"]),
        "dependency_nodes": len(document["dependencies"]),
    }


def _inputs(manifest_path: Path) -> tuple[dict[str, object], dict[str, object], str]:
    metadata = cargo_metadata(manifest_path)
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
        child.add_argument("--manifest-path", type=Path, default=DEFAULT_MANIFEST)
        destination = child.add_mutually_exclusive_group(required=True)
        destination.add_argument("--output", type=Path)
        destination.add_argument("--check", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        metadata, lock, lock_sha256 = _inputs(args.manifest_path)
        if args.command == "notice":
            rendered, summary = render_notice(metadata, lock, lock_sha256)
        else:
            rendered, summary = render_sbom(metadata, lock, lock_sha256)
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
        json.JSONDecodeError,
        tomllib.TOMLDecodeError,
    ) as error:
        print(f"render supply chain: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
