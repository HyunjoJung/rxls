#!/usr/bin/env python3
"""Check rxls's deterministic public-API snapshot using pinned stable rustdoc.

The checker deliberately uses rustdoc HTML instead of unstable rustdoc JSON, so
it runs on the project's Rust 1.85 MSRV. It records public item declarations,
inherent callable items, and explicit trait implementations reachable from the
crate's public module indexes. Compiler-generated auto/blanket implementations
and documentation prose are excluded from the compatibility snapshot.
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
from html.parser import HTMLParser
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
SCHEMA = "rxls.public-api.v1"
DEFAULT_BASELINE = ROOT / "tests" / "oracles" / "public-api-0.1.2.json"
DEFAULT_TARGET_DIR = ROOT / "target" / "public-api-doc"
DEFAULT_TOOLCHAIN = "1.85.0"
ITEM_KINDS = {
    "constant",
    "enum",
    "fn",
    "macro",
    "primitive",
    "static",
    "struct",
    "trait",
    "type",
    "union",
}
INDEX_SECTIONS = {
    "constants",
    "enums",
    "functions",
    "macros",
    "modules",
    "primitives",
    "statics",
    "structs",
    "traits",
    "types",
    "unions",
}
DOC_CONTRACTS = (
    ("rxls::Workbook::open", "struct.Workbook.html", "method.open", ("Errors", "Panics", "Examples")),
    (
        "rxls::Workbook::to_xlsx_checked",
        "struct.Workbook.html",
        "method.to_xlsx_checked",
        ("Errors", "Examples"),
    ),
    ("rxls::extract_text", "fn.extract_text.html", None, ("Errors",)),
    ("rxls::export_csv", "fn.export_csv.html", None, ("Errors", "Examples")),
    ("rxls::Workbook::evaluate_cell", "struct.Workbook.html", "method.evaluate_cell", ("Examples",)),
    (
        "rxls::WorkbookReport::from_workbook",
        "struct.WorkbookReport.html",
        "method.from_workbook",
        ("Examples",),
    ),
    ("rxls::wasm::extract_text_bytes", "wasm/fn.extract_text_bytes.html", None, ("Errors",)),
    ("rxls::wasm::to_csv_bytes", "wasm/fn.to_csv_bytes.html", None, ("Errors",)),
    ("rxls::wasm::to_html_bytes", "wasm/fn.to_html_bytes.html", None, ("Errors",)),
    ("rxls::wasm::report_json_bytes", "wasm/fn.report_json_bytes.html", None, ("Errors",)),
)


@dataclass(frozen=True)
class IndexEntry:
    kind: str
    path: str
    href: str


class ModuleIndexParser(HTMLParser):
    """Collect only direct `<dt>` entries from rustdoc module item tables."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.section: str | None = None
        self.dt_depth = 0
        self.item_name_depth = 0
        self.entries: list[IndexEntry] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = dict(attrs)
        if tag == "h2":
            self.section = values.get("id")
            return
        if tag == "dt":
            self.dt_depth += 1
            return
        if tag == "div" and "item-name" in set((values.get("class") or "").split()):
            self.item_name_depth += 1
            return
        if tag == "div" and self.item_name_depth:
            self.item_name_depth += 1
            return
        if (
            tag != "a"
            or self.section not in INDEX_SECTIONS
            or (self.dt_depth == 0 and self.item_name_depth == 0)
        ):
            return
        classes = set((values.get("class") or "").split())
        kind = next((candidate for candidate in classes if candidate in ITEM_KINDS or candidate == "mod"), None)
        href = values.get("href")
        title = values.get("title")
        if kind is None or not href or not title:
            return
        prefix = "mod " if kind == "mod" else f"{kind} "
        if not title.startswith(prefix):
            return
        self.entries.append(IndexEntry(kind="module" if kind == "mod" else kind, path=title[len(prefix) :], href=href))

    def handle_endtag(self, tag: str) -> None:
        if tag == "dt" and self.dt_depth:
            self.dt_depth -= 1
        elif tag == "div" and self.item_name_depth:
            self.item_name_depth -= 1


class FirstDeclarationParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.capture_depth = 0
        self.complete = False
        self.parts: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if self.complete:
            return
        classes = set((dict(attrs).get("class") or "").split())
        if self.capture_depth == 0 and tag == "pre" and "item-decl" in classes:
            self.capture_depth = 1
        elif self.capture_depth:
            self.capture_depth += 1

    def handle_endtag(self, _tag: str) -> None:
        if not self.capture_depth:
            return
        self.capture_depth -= 1
        if self.capture_depth == 0:
            self.complete = True

    def handle_data(self, data: str) -> None:
        if self.capture_depth:
            self.parts.append(data)


class CodeHeaderParser(HTMLParser):
    def __init__(self, header_tag: str) -> None:
        super().__init__(convert_charrefs=True)
        self.header_tag = header_tag
        self.capture_depth = 0
        self.current: list[str] = []
        self.headers: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        classes = set((dict(attrs).get("class") or "").split())
        if self.capture_depth == 0 and tag == self.header_tag and "code-header" in classes:
            self.capture_depth = 1
            self.current = []
        elif self.capture_depth:
            self.capture_depth += 1

    def handle_endtag(self, _tag: str) -> None:
        if not self.capture_depth:
            return
        self.capture_depth -= 1
        if self.capture_depth == 0:
            self.headers.append(normalize_signature("".join(self.current)))

    def handle_data(self, data: str) -> None:
        if self.capture_depth:
            self.current.append(data)


def normalize_signature(value: str) -> str:
    value = re.sub(r"Show\s+\d+\s+(?:fields|methods|variants)", "", value)
    return " ".join(value.split())


def parse_module_index(path: Path) -> list[IndexEntry]:
    parser = ModuleIndexParser()
    parser.feed(path.read_text(encoding="utf-8"))
    return parser.entries


def first_declaration(html: str) -> str:
    parser = FirstDeclarationParser()
    parser.feed(html)
    declaration = normalize_signature("".join(parser.parts))
    if not parser.complete or not declaration:
        raise ValueError("rustdoc item page has no public declaration")
    return declaration


def _section(html: str, start_id: str, end_ids: tuple[str, ...]) -> str:
    marker = f'id="{start_id}"'
    start = html.find(marker)
    if start < 0:
        return ""
    ends = [html.find(f'id="{end_id}"', start + len(marker)) for end_id in end_ids]
    ends = [end for end in ends if end >= 0]
    return html[start : min(ends) if ends else len(html)]


def code_headers(html: str, section_id: str, end_ids: tuple[str, ...], tag: str) -> list[str]:
    parser = CodeHeaderParser(tag)
    parser.feed(_section(html, section_id, end_ids))
    return sorted(set(header for header in parser.headers if header))


def collect_public_api(doc_root: Path, crate: str = "rxls", toolchain: str = DEFAULT_TOOLCHAIN) -> dict:
    root_index = doc_root / "index.html"
    if not root_index.is_file():
        raise FileNotFoundError(root_index)

    modules: list[tuple[str, Path]] = [(crate, root_index)]
    visited_modules: set[Path] = set()
    items: dict[tuple[str, str], dict[str, object]] = {}
    while modules:
        module_path, index_path = modules.pop()
        resolved_index = index_path.resolve()
        if resolved_index in visited_modules:
            continue
        visited_modules.add(resolved_index)
        for entry in parse_module_index(index_path):
            linked = (index_path.parent / PurePosixPath(entry.href)).resolve()
            key = (entry.path, entry.kind)
            if entry.kind == "module":
                items[key] = {
                    "path": entry.path,
                    "kind": "module",
                    "declaration": f"pub mod {entry.path.rsplit('::', 1)[-1]}",
                    "inherent_items": [],
                    "trait_implementations": [],
                }
                modules.append((entry.path, linked))
                continue
            html = linked.read_text(encoding="utf-8")
            items[key] = {
                "path": entry.path,
                "kind": entry.kind,
                "declaration": first_declaration(html),
                "inherent_items": code_headers(
                    html,
                    "implementations-list",
                    ("trait-implementations", "synthetic-implementations", "blanket-implementations"),
                    "h4",
                ),
                "trait_implementations": code_headers(
                    html,
                    "trait-implementations-list",
                    ("synthetic-implementations", "blanket-implementations"),
                    "h3",
                ),
            }

    ordered = [items[key] for key in sorted(items)]
    counts = Counter(str(item["kind"]) for item in ordered)
    return {
        "schema": SCHEMA,
        "generator": {
            "script": "scripts/check_public_api.py",
            "rust_toolchain": toolchain,
            "surface": "all-features rustdoc public module indexes",
        },
        "crate": crate,
        "summary": {"items": len(ordered), "by_kind": dict(sorted(counts.items()))},
        "items": ordered,
    }


def _method_doc_segment(html: str, fragment: str) -> str:
    marker = f'id="{fragment}"'
    start = html.find(marker)
    if start < 0:
        return ""
    end = html.find("</details>", start)
    return html[start : end if end >= 0 else len(html)]


def check_doc_contracts(doc_root: Path) -> list[str]:
    errors: list[str] = []
    for api_path, relative, fragment, headings in DOC_CONTRACTS:
        page = doc_root / PurePosixPath(relative)
        if not page.is_file():
            errors.append(f"{api_path}: missing rustdoc page {relative}")
            continue
        html = page.read_text(encoding="utf-8")
        segment = _method_doc_segment(html, fragment) if fragment else html
        if not segment:
            errors.append(f"{api_path}: missing rustdoc fragment {fragment}")
            continue
        for heading in headings:
            if re.search(rf"{re.escape(heading)}\s*</h[1-6]>", segment) is None:
                errors.append(f"{api_path}: missing #{heading} documentation")
    return errors


def compare_documents(expected: dict, actual: dict) -> list[str]:
    errors: list[str] = []
    for field in ("schema", "crate", "generator"):
        if expected.get(field) != actual.get(field):
            errors.append(f"{field}: expected {expected.get(field)!r}, found {actual.get(field)!r}")

    def keyed(document: dict) -> dict[tuple[str, str], dict]:
        return {(item["path"], item["kind"]): item for item in document.get("items", [])}

    old = keyed(expected)
    new = keyed(actual)
    for key in sorted(old.keys() - new.keys()):
        errors.append(f"removed public {key[1]}: {key[0]}")
    for key in sorted(new.keys() - old.keys()):
        errors.append(f"added public {key[1]}: {key[0]}")
    for key in sorted(old.keys() & new.keys()):
        before = old[key]
        after = new[key]
        for field in ("declaration", "inherent_items", "trait_implementations"):
            if before.get(field) != after.get(field):
                errors.append(f"changed {key[0]} {field}")
    return errors


def build_docs(manifest_path: Path, target_dir: Path, toolchain: str) -> Path:
    command = [
        "cargo",
        f"+{toolchain}",
        "doc",
        "--manifest-path",
        str(manifest_path),
        "--target-dir",
        str(target_dir),
        "--no-deps",
        "--all-features",
        "--locked",
    ]
    environment = os.environ.copy()
    flags = environment.get("RUSTDOCFLAGS", "")
    if "-D warnings" not in flags:
        environment["RUSTDOCFLAGS"] = (flags + " -D warnings").strip()
    subprocess.run(command, cwd=ROOT, env=environment, check=True)
    return target_dir / "doc" / "rxls"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    parser.add_argument("--manifest-path", type=Path, default=ROOT / "Cargo.toml")
    parser.add_argument("--target-dir", type=Path, default=DEFAULT_TARGET_DIR)
    parser.add_argument("--toolchain", default=DEFAULT_TOOLCHAIN)
    parser.add_argument("--doc-root", type=Path, help="inspect existing rustdoc output instead of building")
    parser.add_argument("--write-baseline", action="store_true")
    parser.add_argument("--print-current", action="store_true")
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)

    try:
        doc_root = args.doc_root or build_docs(args.manifest_path, args.target_dir, args.toolchain)
        current = collect_public_api(doc_root, toolchain=args.toolchain)
        doc_errors = check_doc_contracts(doc_root)
        rendered = json.dumps(current, indent=2, sort_keys=True) + "\n"
        if args.print_current:
            sys.stdout.write(rendered)
        if args.write_baseline:
            args.baseline.parent.mkdir(parents=True, exist_ok=True)
            args.baseline.write_text(rendered, encoding="utf-8")
        elif not args.baseline.is_file():
            doc_errors.append(f"missing public API baseline: {args.baseline}")
        else:
            expected = json.loads(args.baseline.read_text(encoding="utf-8"))
            doc_errors.extend(compare_documents(expected, current))
    except (OSError, ValueError, KeyError, json.JSONDecodeError, subprocess.CalledProcessError) as error:
        print(f"public API: {error}", file=sys.stderr)
        return 2

    if doc_errors:
        for error in doc_errors:
            print(f"public API: {error}", file=sys.stderr)
        return 1
    print(f"public API: verified {current['summary']['items']} items with Rust {args.toolchain}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
