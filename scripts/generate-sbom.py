#!/usr/bin/env python3
"""Generate a deterministic CycloneDX JSON dependency manifest from Cargo metadata."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


def cargo_metadata(manifest_path: Path) -> dict[str, object]:
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--all-features",
        "--manifest-path",
        str(manifest_path),
    ]
    completed = subprocess.run(command, check=True, capture_output=True, text=True)
    return json.loads(completed.stdout)


def package_ref(package: dict[str, object]) -> str:
    """Return a machine-independent identity for a Cargo package."""
    return f"pkg:cargo/{package['name']}@{package['version']}"


def component(package: dict[str, object]) -> dict[str, object]:
    name = str(package["name"])
    version = str(package["version"])
    purl = package_ref(package)
    item: dict[str, object] = {
        "type": "library",
        "bom-ref": purl,
        "name": name,
        "version": version,
        "purl": purl,
    }
    license_expression = package.get("license")
    if license_expression:
        item["licenses"] = [{"expression": str(license_expression)}]
    source = package.get("source")
    if source:
        item["properties"] = [{"name": "cargo:source", "value": str(source)}]
    return item


def make_sbom(metadata: dict[str, object] | list[dict[str, object]]) -> dict[str, object]:
    metadatas = metadata if isinstance(metadata, list) else [metadata]
    packages_by_ref: dict[str, dict[str, object]] = {}
    root_refs: set[str] = set()
    for document in metadatas:
        workspace_ids = set(document.get("workspace_members", []))
        for package in document.get("packages", []):
            ref = package_ref(package)
            packages_by_ref.setdefault(ref, package)
            if package.get("id") in workspace_ids:
                root_refs.add(ref)

    roots = [packages_by_ref[ref] for ref in sorted(root_refs)]
    dependencies = [
        package for ref, package in packages_by_ref.items() if ref not in root_refs
    ]
    dependencies.sort(
        key=lambda package: (
            str(package["name"]),
            str(package["version"]),
            package_ref(package),
        )
    )

    document: dict[str, object] = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "components": [component(package) for package in dependencies],
    }
    if len(roots) == 1:
        document["metadata"] = {
            "component": component(roots[0]),
            "properties": [
                {"name": "rxls:generator", "value": "scripts/generate-sbom.py"},
                {"name": "rxls:locked", "value": "true"},
            ],
        }
    elif roots:
        versions = {str(package["version"]) for package in roots}
        version = versions.pop() if len(versions) == 1 else "mixed"
        distribution_ref = f"pkg:generic/rxls-distribution@{version}"
        document["metadata"] = {
            "component": {
                "type": "application",
                "bom-ref": distribution_ref,
                "name": "rxls-distribution",
                "version": version,
                "purl": distribution_ref,
                "components": [component(package) for package in roots],
            },
            "properties": [
                {"name": "rxls:generator", "value": "scripts/generate-sbom.py"},
                {"name": "rxls:locked", "value": "true"},
                {"name": "rxls:manifests", "value": str(len(metadatas))},
            ],
        }
    return document


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", type=Path, action="append")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        manifests = args.manifest_path or [Path("Cargo.toml")]
        payload = make_sbom([cargo_metadata(path) for path in manifests])
        rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered, encoding="utf-8")
        else:
            sys.stdout.write(rendered)
    except (OSError, ValueError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"generate-sbom: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
