#!/usr/bin/env python3
"""Run fresh edition-2021 full and minimal consumers on the core Rust MSRV."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
TOOLCHAIN = "1.88.0"
TIMEOUT = 30 * 60
SOURCE = r'''fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert!(rxls::Workbook::open(b"invalid spreadsheet").is_err());
    #[cfg(feature = "full")]
    {
        use rxls::{Cell, Spreadsheet, Workbook};
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("Data");
        sheet.write(0, 0, "fresh Rust consumer");
        sheet.write(1, 0, 42.0);
        let bytes = workbook.to_xlsx_checked()?;
        let reopened = Workbook::open(&bytes)?;
        assert_eq!(reopened.sheets[0].cell(1, 0), Some(&Cell::Number(42.0)));
        let mut editable = Spreadsheet::open(&bytes)?;
        editable.set_cell_value("Data", 1, 0, Cell::Number(84.0))?;
        let edited = Workbook::open(&editable.save()?)?;
        assert_eq!(edited.sheets[0].cell(1, 0), Some(&Cell::Number(84.0)));
        assert!(edited.text().contains("fresh Rust consumer"));
        println!("full consumer: read, write, and edit passed");
    }
    #[cfg(not(feature = "full"))]
    println!("minimal consumer: invalid input rejection passed");
    Ok(())
}
'''


def run(arguments: list[str], *, cwd: Path, env: dict[str, str]) -> str:
    result = subprocess.run(
        arguments, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE,
        check=True, timeout=TIMEOUT,
    )
    return result.stdout.strip()


def check(source: Path, target: Path) -> dict[str, object]:
    package = tomllib.loads((source / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    if package["name"] != "rxls" or package.get("rust-version") != "1.88":
        raise ValueError("fresh consumers must target rxls with the reviewed Rust 1.88 floor")
    env = dict(os.environ)
    # These consumers must not silently fall back to an older dependency graph.
    # An edition-2021 application with Cargo's ordinary resolver must work.
    env.pop("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS", None)
    cargo = ["cargo", f"+{TOOLCHAIN}", "--config", 'resolver.incompatible-rust-versions="allow"']
    rustc = run(["rustc", f"+{TOOLCHAIN}", "--version"], cwd=source, env=env)
    if not rustc.startswith(f"rustc {TOOLCHAIN} "):
        raise ValueError("fresh consumer compiler differs from the reviewed MSRV")
    results = []
    with tempfile.TemporaryDirectory(prefix="rxls-fresh-consumers-") as temporary:
        outside = Path(temporary).resolve()
        if outside.is_relative_to(source):
            raise ValueError("fresh consumer directory must be outside the source checkout")
        for profile in ("full", "minimal"):
            consumer = outside / profile
            (consumer / "src").mkdir(parents=True)
            (consumer / "Cargo.toml").write_text(
                '[package]\nname = "rxls-fresh-consumer"\nversion = "0.0.0"\n'
                'edition = "2021"\nrust-version = "1.88"\n\n'
                '[workspace]\n\n[features]\nfull = ["rxls/full"]\n\n'
                '[dependencies.rxls]\ndefault-features = false\n'
                f'version = {json.dumps("=" + package["version"])}\n'
                f'path = {json.dumps(str(source))}\n',
                encoding="utf-8",
            )
            (consumer / "src/main.rs").write_text(SOURCE, encoding="utf-8")
            manifest = str(consumer / "Cargo.toml")
            features = ["--no-default-features"]
            if profile == "full":
                features += ["--features", "full"]
            # The directory has no lockfile: metadata resolves an independent,
            # current graph instead of inheriting the library's checked lock.
            metadata = json.loads(run(
                cargo + ["metadata", "--format-version", "1", "--manifest-path", manifest] + features,
                cwd=consumer, env=env,
            ))
            lock_before = (consumer / "Cargo.lock").read_bytes()
            output = run(
                cargo + ["run", "--locked", "--manifest-path", manifest,
                         "--target-dir", str(target)] + features,
                cwd=consumer, env=env,
            )
            if (consumer / "Cargo.lock").read_bytes() != lock_before:
                raise ValueError("consumer execution changed the newly resolved lock")
            results.append({
                "profile": profile,
                "lock_sha256": hashlib.sha256(lock_before).hexdigest(),
                "output": output,
                "dependencies": sorted(
                    ({"name": item["name"], "version": item["version"]}
                     for item in metadata["packages"] if item["name"] != "rxls-fresh-consumer"),
                    key=lambda item: (item["name"], item["version"]),
                ),
            })
    return {
        "schema": "rxls.fresh-consumers.v1",
        "source_version": package["version"],
        "rustc": rustc,
        "edition": "2021",
        "profiles": results,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, default=ROOT)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target/fresh-consumers")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    try:
        report = check(arguments.source_root.resolve(), arguments.target_dir.resolve())
        rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if arguments.output:
            arguments.output.parent.mkdir(parents=True, exist_ok=True)
            arguments.output.write_text(rendered, encoding="utf-8")
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"Fresh consumer check failed: {error}")
        return 1
    print(f"Fresh edition-2021 full and minimal consumers passed on Rust {TOOLCHAIN}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
