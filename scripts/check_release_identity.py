#!/usr/bin/env python3
"""Validate source package relationships without coupling independent releases.

Package versions remain in their owning manifests. package-identities.json records
only names, local dependency edges, shared version groups, and supported Rust floors.
Artifact provenance and packed-file checks remain separate release gates.
"""

from __future__ import annotations

import json
from pathlib import Path
import posixpath
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
POLICY = "scripts/package-identities.json"
SEMVER = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def _load(path: Path) -> dict:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def _load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def _path_package_version(lock: dict, name: str) -> str | None:
    matches = [
        package
        for package in lock.get("package", [])
        if package.get("name") == name and "source" not in package
    ]
    if len(matches) != 1:
        return None
    return matches[0].get("version")


def _validate_package_graph(root: Path) -> list[str]:
    policy = _load_json(root / POLICY)
    if policy.get("schema") != "rxls.package-identities.v1":
        raise ValueError(f"{POLICY}: unsupported schema")
    cargo = policy["cargo"]
    manifests = {name: _load(root / rule["manifest"]) for name, rule in cargo.items()}
    locks = {
        name: _load((root / rule["manifest"]).with_name("Cargo.lock"))
        for name, rule in cargo.items()
    }
    versions = {
        name: manifests[rule.get("version_from", name)]["package"].get("version")
        for name, rule in cargo.items()
    }
    errors: list[str] = []

    def same(label: str, actual: object, expected: object) -> None:
        if actual != expected:
            found = actual if actual is not None else "missing/ambiguous"
            errors.append(f"{label}: expected {expected}, found {found}")

    def valid_version(label: str, version: object) -> None:
        if not isinstance(version, str) or SEMVER.fullmatch(version) is None:
            errors.append(f"{label}: expected a package SemVer, found {version}")

    for name, rule in cargo.items():
        relative = rule["manifest"]
        package = manifests[name]["package"]
        same(f"{relative} name", package.get("name"), name)
        valid_version(f"{relative} version", package.get("version"))
        same(f"{relative} version", package.get("version"), versions[name])
        same(
            f"{relative} rust-version", package.get("rust-version"),
            policy["rust_floors"][rule["rust_floor"]],
        )
        if "publish" in rule:
            same(f"{relative} publish", package.get("publish"), rule["publish"])
        for dependency_name in rule["dependencies"]:
            dependency = manifests[name].get("dependencies", {}).get(dependency_name, {})
            if not isinstance(dependency, dict):
                dependency = {}
            target = cargo[dependency_name]["manifest"]
            expected_path = posixpath.relpath(
                posixpath.dirname(target) or ".", posixpath.dirname(relative) or "."
            )
            same(
                f"{relative} {dependency_name} dependency path",
                dependency.get("path"), expected_path,
            )
            same(
                f"{relative} {dependency_name} dependency version",
                dependency.get("version"), versions[dependency_name],
            )

        # Include transitive local packages; registry packages with the same name
        # cannot stand in for the unique source-less path identity.
        pending, members = [name], set()
        while pending:
            member = pending.pop()
            if member not in members:
                members.add(member)
                pending.extend(cargo[member]["dependencies"])
        lock_relative = posixpath.join(posixpath.dirname(relative), "Cargo.lock")
        for member in sorted(members):
            same(
                f"{lock_relative} {member}",
                _path_package_version(locks[name], member), versions[member],
            )

    for name, rule in policy["npm"].items():
        relative = rule["manifest"]
        manifest = _load_json(root / relative)
        same(f"{relative} name", manifest.get("name"), name)
        valid_version(f"{relative} version", manifest.get("version"))
        if "version_from" in rule:
            same(
                f"{relative} version", manifest.get("version"),
                versions[rule["version_from"]],
            )
        if "lockfile" in rule:
            lock = _load_json(root / rule["lockfile"])
            package = lock.get("packages", {}).get("", {})
            for field in ("name", "version"):
                same(f"{rule['lockfile']} {field}", lock.get(field), manifest.get(field))
                same(
                    f"{rule['lockfile']} packages[''] {field}",
                    package.get(field), manifest.get(field),
                )

    toolchain_rule = policy["wasm_toolchain"]
    toolchain_relative = toolchain_rule["lockfile"]
    toolchain = _load_json(root / toolchain_relative)
    same(
        f"{toolchain_relative} rust", toolchain.get("rust"),
        policy["rust_floors"][toolchain_rule["rust_floor"]] + ".0",
    )
    bindgen_version = toolchain["wasmBindgen"]["version"]
    valid_version(f"{toolchain_relative} wasmBindgen.version", bindgen_version)
    for name in toolchain_rule["packages"]:
        relative = cargo[name]["manifest"]
        dependency = manifests[name].get("dependencies", {}).get("wasm-bindgen")
        if isinstance(dependency, dict):
            dependency = dependency.get("version")
        same(f"{relative} wasm-bindgen", dependency, bindgen_version)
        resolved = [
            entry.get("version") for entry in locks[name].get("package", [])
            if entry.get("name") == "wasm-bindgen"
        ]
        lock_relative = posixpath.join(posixpath.dirname(relative), "Cargo.lock")
        same(f"{lock_relative} wasm-bindgen", resolved, [bindgen_version])

    return errors


def validate(root: Path = ROOT) -> list[str]:
    errors = _validate_package_graph(root)
    root_manifest = _load(root / "Cargo.toml")
    wasm_manifest = _load(root / "bindings" / "wasm" / "Cargo.toml")
    npm_manifest = _load_json(root / "bindings" / "wasm" / "npm" / "package.json")
    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
    version = root_manifest["package"]["version"]

    dependency = wasm_manifest.get("dependencies", {}).get("rxls", {})
    if dependency.get("default-features") is not False:
        errors.append("WASM rxls dependency must set default-features = false")

    if npm_manifest.get("private") is True:
        errors.append("WASM npm package must remain publishable")
    expected_entries = {
        "main": "./node/rxls_wasm.js",
        "types": "./node/rxls_wasm.d.ts",
    }
    for field, expected in expected_entries.items():
        if npm_manifest.get(field) != expected:
            errors.append(f"WASM npm {field}: expected {expected}")
    if "module" in npm_manifest or "browser" in npm_manifest:
        errors.append("WASM npm must use conditional exports for browser selection")
    if npm_manifest.get("engines", {}).get("node") != ">=20":
        errors.append("WASM npm engines.node must be >=20")
    required_files = {"node", "web", "demo", "README.md", "LICENSE"}
    if not required_files.issubset(set(npm_manifest.get("files", []))):
        errors.append("WASM npm files must include runtime, demo, docs, and license")

    changelog_checks = {
        "release heading": f"## [{version}]",
        "release link": f"[{version}]: https://github.com/HyunjoJung/rxls/releases/tag/v{version}",
        "Unreleased comparison": (
            f"[Unreleased]: https://github.com/HyunjoJung/rxls/compare/v{version}...HEAD"
        ),
    }
    for label, expected in changelog_checks.items():
        if expected not in changelog:
            errors.append(f"CHANGELOG {label}: expected {expected!r}")

    return errors


def main() -> int:
    try:
        errors = validate()
        root_manifest = _load(ROOT / "Cargo.toml")
    except (OSError, KeyError, ValueError, TypeError) as error:
        print(f"release identity: {error}", file=sys.stderr)
        return 2

    if errors:
        for error in errors:
            print(f"release identity: {error}", file=sys.stderr)
        return 1

    package = root_manifest["package"]
    print(
        "release identity: "
        f"version={package['version']} rust-version={package['rust-version']} "
        "native=ok wasm=ok npm=ok locks=ok package-graph=ok toolchain=ok"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
