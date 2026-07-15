#!/usr/bin/env python3
"""Validate that native, WASM, and lockfile release identities stay in sync."""

from __future__ import annotations

import json
from pathlib import Path
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]


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


def validate(root: Path = ROOT) -> list[str]:
    root_manifest = _load(root / "Cargo.toml")
    root_lock = _load(root / "Cargo.lock")
    wasm_manifest = _load(root / "bindings" / "wasm" / "Cargo.toml")
    wasm_lock = _load(root / "bindings" / "wasm" / "Cargo.lock")
    npm_manifest = _load_json(root / "bindings" / "wasm" / "npm" / "package.json")
    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")

    version = root_manifest["package"]["version"]
    rust_version = root_manifest["package"]["rust-version"]
    errors: list[str] = []

    checks = {
        "root Cargo.lock rxls": _path_package_version(root_lock, "rxls"),
        "WASM Cargo.toml rxls-wasm": wasm_manifest["package"]["version"],
        "WASM Cargo.lock rxls": _path_package_version(wasm_lock, "rxls"),
        "WASM Cargo.lock rxls-wasm": _path_package_version(wasm_lock, "rxls-wasm"),
        "WASM npm package.json": npm_manifest.get("version"),
    }
    for label, actual in checks.items():
        if actual != version:
            errors.append(f"{label}: expected {version}, found {actual or 'missing/ambiguous'}")

    wasm_rust_version = wasm_manifest["package"].get("rust-version")
    if wasm_rust_version != rust_version:
        errors.append(
            "WASM Cargo.toml rust-version: "
            f"expected {rust_version}, found {wasm_rust_version or 'missing'}"
        )

    dependency = wasm_manifest.get("dependencies", {}).get("rxls", {})
    if dependency.get("path") != "../..":
        errors.append("WASM rxls dependency must use path = '../..'")
    if dependency.get("default-features") is not False:
        errors.append("WASM rxls dependency must set default-features = false")
    if wasm_manifest["package"].get("publish") is not False:
        errors.append("rxls-wasm must remain publish = false")

    if npm_manifest.get("name") != "rxls-wasm":
        errors.append("WASM npm package name must be rxls-wasm")
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
    except (OSError, KeyError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
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
        "native=ok wasm=ok npm=ok locks=ok"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
