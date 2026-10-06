#!/usr/bin/env python3
"""Filesystem regression tests for the shared public-corpus selectors."""

from __future__ import annotations

import errno
import json
import os
import tempfile
import unittest
from contextlib import chdir
from pathlib import Path
from unittest.mock import MagicMock, patch

from public_corpus_manifest import corpus_files, manifest_files


class CorpusSelectionTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        # getcwd() resolves macOS /var -> /private/var when a test changes cwd.
        self.root = Path(temporary.name).resolve()

    def make_file(self, name: str) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"fixture")
        return path

    def make_manifest(self, entries: list[dict], *, wrapped: bool = True) -> Path:
        path = self.root / "manifest.json"
        path.write_text(
            json.dumps({"files": entries} if wrapped else entries), encoding="utf-8"
        )
        return path

    def test_flat_selection_is_case_insensitive_for_all_supported_formats(self) -> None:
        names = ["a.XLS", "b.XlSx", "c.XLSM", "d.XlSb", "e.ODS"]
        expected = sorted(str(self.make_file(name)) for name in names)
        self.assertEqual(
            corpus_files(self.root, {".xls", ".xlsx", ".xlsm", ".xlsb", ".ods"}),
            expected,
        )

    def test_flat_root_is_literal_not_a_glob_pattern(self) -> None:
        selected = self.make_file("batch[1]/book.xls")
        self.make_file("batch1/decoy.xls")
        self.assertEqual(corpus_files(selected.parent, {".xls"}), [str(selected)])

    def test_flat_extension_aliases_do_not_duplicate_or_consume_limit(self) -> None:
        first = self.make_file("a.xls")
        second = self.make_file("b.xls")
        extensions = (value for value in ("xls", ".XLS", ".xls"))
        self.assertEqual(
            corpus_files(self.root, extensions, limit=2), [str(first), str(second)]
        )

    def test_flat_directories_do_not_consume_limit(self) -> None:
        (self.root / "a.xls").mkdir()
        selected = self.make_file("b.xls")
        self.assertEqual(corpus_files(self.root, {".xls"}, limit=1), [str(selected)])

    def test_flat_selection_stays_nonrecursive_and_excludes_hidden_files(self) -> None:
        selected = self.make_file("book.xls")
        self.make_file(".hidden.xls")
        self.make_file("nested/other.xls")
        self.make_file("ignored.txt")
        self.assertEqual(corpus_files(self.root, {".xls"}), [str(selected)])

    def test_flat_selection_sorts_before_applying_limit(self) -> None:
        expected = sorted(
            str(self.make_file(name)) for name in ["c.xls", "a.xls", "b.xls"]
        )
        self.assertEqual(corpus_files(self.root, {".xls"}), expected)
        self.assertEqual(corpus_files(self.root, {".xls"}, limit=2), expected[:2])
        self.assertEqual(corpus_files(self.root, {".xls"}, limit=0), [])

    def test_flat_extensions_are_literal_suffixes(self) -> None:
        self.make_file("book.xls")
        self.make_file("book.xlsx")
        for extension in (".x*", ".xl?", ".xl[st]"):
            with self.subTest(extension=extension):
                self.assertEqual(corpus_files(self.root, {extension}), [])

    def test_flat_missing_or_nondirectory_root_selects_nothing(self) -> None:
        selected = self.make_file("book.xls")
        self.assertEqual(corpus_files(self.root / "missing", {".xls"}), [])
        self.assertEqual(corpus_files(selected, {".xls"}), [])

    def test_flat_scan_open_errors_are_not_reported_as_empty_corpora(self) -> None:
        for error in (PermissionError(errno.EACCES, "denied"), OSError(errno.EIO, "I/O")):
            with self.subTest(error=type(error).__name__):
                with patch("public_corpus_manifest.os.scandir", side_effect=error):
                    with self.assertRaises(type(error)) as raised:
                        corpus_files(self.root, {".xls"})
                self.assertIs(raised.exception, error)

    def test_flat_scan_iteration_errors_do_not_return_partial_selection(self) -> None:
        entry = MagicMock()
        entry.name = "a.xls"
        entry.is_file.return_value = True
        error = OSError(errno.EIO, "I/O")

        def entries():
            yield entry
            raise error

        with patch("public_corpus_manifest.os.scandir") as scan:
            scan.return_value.__enter__.return_value = entries()
            with self.assertRaises(OSError) as raised:
                corpus_files(self.root, {".xls"}, limit=1)
        self.assertIs(raised.exception, error)
        entry.is_file.assert_called_once_with()

    def test_flat_entry_stat_errors_are_not_silently_skipped(self) -> None:
        entry = MagicMock()
        entry.name = "a.xls"
        error = PermissionError(errno.EACCES, "denied")
        entry.is_file.side_effect = error
        with patch("public_corpus_manifest.os.scandir") as scan:
            scan.return_value.__enter__.return_value = iter([entry])
            with self.assertRaises(PermissionError) as raised:
                corpus_files(self.root, {".xls"})
        self.assertIs(raised.exception, error)

    def test_flat_relative_root_preserves_relative_output_paths(self) -> None:
        self.make_file("payload/book.xls")
        with chdir(self.root):
            self.assertEqual(
                corpus_files("payload", {".xls"}), [os.path.join("payload", "book.xls")]
            )

    def test_flat_empty_root_keeps_current_directory_behavior(self) -> None:
        self.make_file("book.xls")
        with chdir(self.root):
            self.assertEqual(corpus_files("", {".xls"}), ["book.xls"])

    def test_flat_empty_extensions_select_nothing(self) -> None:
        self.make_file("book.xls")
        self.assertEqual(corpus_files(self.root, iter(())), [])

    def test_flat_symlinks_select_only_regular_file_targets(self) -> None:
        selected = self.make_symlink_fixture()
        self.assertEqual(corpus_files(self.root, {".xls"}), [str(selected)])

    def test_manifest_accepts_case_insensitive_extension_aliases(self) -> None:
        selected = self.make_file("book.XLS")
        manifest = self.make_manifest([{"local_path": str(selected), "status": "ready"}])
        self.assertEqual(manifest_files(manifest, iter(["xls"])), [str(selected)])
        self.assertEqual(manifest_files(manifest, {".XLS", "xls"}), [str(selected)])

    def test_manifest_directories_do_not_consume_limit(self) -> None:
        directory = self.root / "a.xls"
        directory.mkdir()
        selected = self.make_file("b.xls")
        manifest = self.make_manifest(
            [{"local_path": str(directory)}, {"local_path": str(selected)}]
        )
        self.assertEqual(manifest_files(manifest, {".xls"}, limit=1), [str(selected)])

    def test_manifest_relative_file_is_not_shadowed_by_cwd_directory(self) -> None:
        selected = self.make_file("payload/book.xls")
        cwd = self.root / "checkout"
        (cwd / "payload" / "book.xls").mkdir(parents=True)
        manifest = self.make_manifest([{"local_path": "payload/book.xls"}])
        with chdir(cwd):
            self.assertEqual(manifest_files(manifest, {".xls"}), [str(selected)])

    def test_manifest_keeps_repository_relative_file_precedence(self) -> None:
        self.make_file("payload/book.xls")
        selected = self.make_file("checkout/payload/book.xls")
        manifest = self.make_manifest([{"local_path": "payload/book.xls"}])
        with chdir(self.root / "checkout"):
            self.assertEqual(manifest_files(manifest, {".xls"}), [str(selected)])

    def test_manifest_keeps_status_eligibility_and_render_selection_filters(self) -> None:
        selected = self.make_file("selected.xls")
        excluded = self.make_file("excluded.xls")
        manifest = self.make_manifest(
            [
                {"local_path": str(selected), "status": "ready"},
                {"local_path": str(excluded), "status": "failed"},
                {"local_path": str(excluded), "status": "planned"},
                {"local_path": str(excluded), "eligible": False},
                {"local_path": str(excluded), "render_selected": False},
                {"local_path": str(self.root / "missing.xls")},
                {},
            ]
        )
        self.assertEqual(manifest_files(manifest, {".xls"}), [str(selected)])

    def test_manifest_deduplicates_and_sorts_before_limit(self) -> None:
        first = self.make_file("a.xls")
        second = self.make_file("b.xls")
        manifest = self.make_manifest(
            [
                {"local_path": str(second), "status": "downloaded"},
                {"local_path": str(first), "status": "cached"},
                {"local_path": str(first), "status": "duplicate"},
            ]
        )
        self.assertEqual(manifest_files(manifest, {".xls"}), [str(first), str(second)])
        self.assertEqual(manifest_files(manifest, {".xls"}, limit=1), [str(first)])
        self.assertEqual(manifest_files(manifest, {".xls"}, limit=0), [])

    def test_manifest_keeps_list_shape_and_manifest_relative_paths(self) -> None:
        selected = self.make_file("payload/book.xls")
        manifest = self.make_manifest([{"local_path": "payload/book.xls"}], wrapped=False)
        cwd = self.root / "checkout"
        cwd.mkdir()
        with chdir(cwd):
            self.assertEqual(manifest_files(manifest, {".xls"}), [str(selected)])

    def test_manifest_keeps_source_extension_for_renamed_local_payloads(self) -> None:
        selected = self.make_file("payload.bin")
        manifest = self.make_manifest(
            [{"path": "upstream/book.XLS", "local_path": str(selected)}]
        )
        self.assertEqual(manifest_files(manifest, {".xls"}), [str(selected)])

    def test_manifest_symlinks_select_only_regular_file_targets(self) -> None:
        selected = self.make_symlink_fixture()
        manifest = self.make_manifest(
            [
                {"local_path": str(self.root / name)}
                for name in ("file.xls", "dir.xls", "broken.xls")
            ]
        )
        self.assertEqual(manifest_files(manifest, {".xls"}), [str(selected)])

    def make_symlink_fixture(self) -> Path:
        target = self.make_file("target.bin")
        selected = self.root / "file.xls"
        try:
            selected.symlink_to(target)
            (self.root / "dir.xls").symlink_to(self.root, target_is_directory=True)
            (self.root / "broken.xls").symlink_to(self.root / "missing")
        except (OSError, NotImplementedError) as error:
            self.skipTest(f"symlinks unavailable: {error}")
        return selected


if __name__ == "__main__":
    unittest.main()
