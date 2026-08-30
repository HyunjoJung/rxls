#!/usr/bin/env python3
"""Structural checks for the public README and guide set."""

from __future__ import annotations

from collections import Counter
from pathlib import Path
import re
import unittest
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[1]
GUIDES = (
    "compatibility.md",
    "preservation.md",
    "validation.md",
    "format-internals.md",
    "formulas.md",
)
PUBLIC_DOCUMENTS = (
    ROOT / "README.md",
    ROOT / "README.ko.md",
    *(ROOT / "docs" / name for name in GUIDES),
)
INLINE_LINK = re.compile(r"!?\[[^\]]*\]\(([^)\s]+)(?:\s+[^)]*)?\)")
HEADING = re.compile(r"^#{1,6}\s+(.+?)\s*#*\s*$", re.MULTILINE)


def _github_anchors(text: str) -> set[str]:
    seen: Counter[str] = Counter()
    anchors: set[str] = set()
    for match in HEADING.finditer(text):
        heading = re.sub(r"<[^>]+>", "", match.group(1))
        heading = heading.replace("`", "")
        slug = heading.casefold()
        slug = re.sub(r"[^\w\s-]", "", slug, flags=re.UNICODE)
        slug = re.sub(r"\s+", "-", slug.strip())
        duplicate = seen[slug]
        seen[slug] += 1
        anchors.add(slug if duplicate == 0 else f"{slug}-{duplicate}")
    return anchors


def _local_links(document: Path, text: str):
    for match in INLINE_LINK.finditer(text):
        raw = match.group(1).strip("<>")
        parsed = urlsplit(raw)
        if parsed.scheme or raw.startswith("//"):
            continue
        path_text = unquote(parsed.path)
        target = document if not path_text else (document.parent / path_text).resolve()
        yield raw, target, unquote(parsed.fragment)


class PublicDocsTests(unittest.TestCase):
    def test_public_document_links_and_anchors_resolve(self) -> None:
        for document in PUBLIC_DOCUMENTS:
            text = document.read_text(encoding="utf-8")
            for raw, target, fragment in _local_links(document, text):
                with self.subTest(document=document.name, link=raw):
                    self.assertTrue(target.exists(), f"{document}: missing {target}")
                    if fragment and target.is_file() and target.suffix.lower() == ".md":
                        anchors = _github_anchors(target.read_text(encoding="utf-8"))
                        self.assertIn(fragment.casefold(), anchors)

    def test_readmes_have_reciprocal_language_navigation(self) -> None:
        english = (ROOT / "README.md").read_text(encoding="utf-8")
        korean = (ROOT / "README.ko.md").read_text(encoding="utf-8")

        self.assertIn("**English** | [한국어](README.ko.md)", english)
        self.assertIn("[English](README.md) | **한국어**", korean)

    def test_both_readmes_index_every_canonical_guide(self) -> None:
        for readme_name in ("README.md", "README.ko.md"):
            text = (ROOT / readme_name).read_text(encoding="utf-8")
            for guide in GUIDES:
                with self.subTest(readme=readme_name, guide=guide):
                    self.assertIn(f"(docs/{guide})", text)

    def test_corpus_markers_have_one_canonical_location_each(self) -> None:
        contents = {
            document.relative_to(ROOT).as_posix(): document.read_text(encoding="utf-8")
            for document in PUBLIC_DOCUMENTS
        }
        locations = {
            marker: [name for name, text in contents.items() if marker in text]
            for marker in (
                "<!-- public-corpus-summary:en:start -->",
                "<!-- public-corpus-summary:ko:start -->",
                "<!-- public-corpus-baseline:start -->",
            )
        }

        self.assertEqual(
            locations["<!-- public-corpus-summary:en:start -->"], ["README.md"]
        )
        self.assertEqual(
            locations["<!-- public-corpus-summary:ko:start -->"], ["README.ko.md"]
        )
        self.assertEqual(
            locations["<!-- public-corpus-baseline:start -->"],
            ["docs/validation.md"],
        )


if __name__ == "__main__":
    unittest.main()
