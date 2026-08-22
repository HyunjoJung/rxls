from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check_public_api.py"


def _load():
    spec = importlib.util.spec_from_file_location("check_public_api", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


INDEX = """<h2 id="modules">Modules</h2><dl class="item-table">
<dt><a class="mod" href="wasm/index.html" title="mod rxls::wasm">wasm</a></dt></dl>
<h2 id="structs">Structs</h2><ul class="item-table"><li>
<div class="item-name"><a class="struct" href="struct.Book.html" title="struct rxls::Book">Book</a></div>
<div class="desc">Documentation may link to <a class="struct" href="private.html" title="struct private::Nope">Nope</a>.</div></li></ul>
<h2 id="functions">Functions</h2><dl class="item-table">
<dt><a class="fn" href="fn.open.html" title="fn rxls::open">open</a></dt></dl>"""

BOOK = """<pre class="rust item-decl"><code>pub struct Book { pub name: String }</code></pre>
<div id="implementations-list"><h4 class="code-header">pub fn new(name: String) -&gt; Self</h4></div>
<h2 id="trait-implementations"></h2><div id="trait-implementations-list">
<h3 class="code-header">impl Clone for Book</h3></div><h2 id="synthetic-implementations"></h2>"""


class PublicApiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = _load()

    def test_collects_public_modules_declarations_and_impls_deterministically(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "wasm").mkdir()
            (root / "index.html").write_text(INDEX, encoding="utf-8")
            (root / "struct.Book.html").write_text(BOOK, encoding="utf-8")
            (root / "fn.open.html").write_text(
                '<pre class="rust item-decl"><code>pub fn open() -&gt; Book</code></pre>',
                encoding="utf-8",
            )
            (root / "wasm" / "index.html").write_text(
                '<h2 id="functions">Functions</h2><dl class="item-table"><dt>'
                '<a class="fn" href="fn.run.html" title="fn rxls::wasm::run">run</a>'
                "</dt></dl>",
                encoding="utf-8",
            )
            (root / "wasm" / "fn.run.html").write_text(
                '<pre class="rust item-decl"><code>pub fn run()</code></pre>', encoding="utf-8"
            )

            first = self.module.collect_public_api(root)
            second = self.module.collect_public_api(root)
            self.assertEqual(first, second)
            self.assertEqual(first["summary"]["items"], 4)
            self.assertNotIn("private::Nope", json.dumps(first))
            book = next(item for item in first["items"] if item["path"] == "rxls::Book")
            self.assertEqual(book["inherent_items"], ["pub fn new(name: String) -> Self"])
            self.assertEqual(book["trait_implementations"], ["impl Clone for Book"])

    def test_compare_reports_added_removed_and_changed_items(self) -> None:
        item = {
            "path": "rxls::open",
            "kind": "fn",
            "declaration": "pub fn open()",
            "inherent_items": [],
            "trait_implementations": [],
        }
        generator = {
            "script": "scripts/check_public_api.py",
            "rust_toolchain": "1.85.0",
            "surface": "all-features rustdoc public module indexes",
        }
        expected = {
            "schema": self.module.SCHEMA,
            "crate": "rxls",
            "generator": generator,
            "items": [item],
        }
        changed = dict(item, declaration="pub fn open(value: u8)")
        added = dict(item, path="rxls::new")
        actual = {
            "schema": self.module.SCHEMA,
            "crate": "rxls",
            "generator": generator,
            "items": [changed, added],
        }
        errors = self.module.compare_documents(expected, actual)
        self.assertIn("changed rxls::open declaration", errors)
        self.assertIn("added public fn: rxls::new", errors)

    def test_normalization_removes_rustdoc_toggle_counts(self) -> None:
        self.assertEqual(
            self.module.normalize_signature("pub trait Read { Show 12 methods fn read(); }"),
            "pub trait Read { fn read(); }",
        )


if __name__ == "__main__":
    unittest.main()
