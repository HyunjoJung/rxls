#!/usr/bin/env python3
"""Tests for the pinned, rights-aware render corpus acquisition tool."""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "fetch-render-corpus.py"
RECIPE = ROOT / "scripts" / "render-corpus-sources.json"


def load_module():
    spec = importlib.util.spec_from_file_location("fetch_render_corpus", SCRIPT)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_package(
    path: Path,
    extra: dict[str, bytes] | None = None,
    *,
    timestamp: tuple[int, int, int, int, int, int] | None = None,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        def add(name: str, payload: bytes) -> None:
            if timestamp is None:
                archive.writestr(name, payload)
            else:
                info = zipfile.ZipInfo(name, timestamp)
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, payload)

        add(
            "[Content_Types].xml",
            b'<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>',
        )
        add("xl/workbook.xml", b"<workbook/>")
        for name, payload in sorted((extra or {}).items()):
            add(name, payload)


class RenderCorpusRecipeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_recipe_pins_reviewed_sources_and_fail_closed_policy(self) -> None:
        recipe = self.module.load_recipe(RECIPE)
        sources = {source["id"]: source for source in recipe["sources"]}

        self.assertEqual(recipe["default_include_tiers"], ["S"])
        self.assertEqual(
            {
                source_id: source["commit"]
                for source_id, source in sources.items()
            },
            {
                "apache-poi-spreadsheets": "86e967d9b28d6a322a87ae8fcbf2a7eeb56cef96",
                "calamine-spreadsheets": "2872ac1c7c02d03fc8549239f5ce629f6b08e54a",
                "closedxml-examples": "4e89dcedd83cad553e84d2d97f77fc3d7deb630f",
                "libreoffice-calc-qa": "fae8a7da316653998a1d177bedeb66dd65694421",
                "libxlsxwriter": "3227eb0c1556ba41d94605a7abee0e097782d10a",
                "odftoolkit-odfdom": "cfd3a9fbbda351fad38aad5e112e3598d08e23f0",
                "openxml-sdk-spreadsheets": "00967dc871f06776ae969762c6703d062308a6c9",
                "phpspreadsheet-templates": "05e99ebf61238a70227b4d9cc02d0030d34f6339",
                "xlsxwriter": "cf3fe78d3eab5e4c7d825d4451af3a60e2a04011",
            },
        )
        self.assertEqual(
            sources["apache-poi-spreadsheets"]["path_scopes"],
            [
                "poi-examples/src/main/java/org/apache/poi/examples/ss/formula/mortgage-calculation.xls",
                "test-data",
            ],
        )
        self.assertEqual(
            sources["calamine-spreadsheets"]["path_scopes"],
            ["tests"],
        )
        self.assertEqual(
            sources["closedxml-examples"]["path_scopes"],
            ["ClosedXML.Tests/Resource/Examples"],
        )
        self.assertEqual(
            sources["libreoffice-calc-qa"]["path_scopes"],
            ["sc/qa/unit/data"],
        )
        self.assertEqual(
            sources["libxlsxwriter"]["path_scopes"],
            ["test/functional/xlsx_files"],
        )
        self.assertEqual(
            sources["odftoolkit-odfdom"]["path_scopes"],
            ["odfdom/src/test/resources/test-input"],
        )
        self.assertEqual(
            sources["openxml-sdk-spreadsheets"]["path_scopes"],
            [
                "test/DocumentFormat.OpenXml.Tests.Assets/assets/TestDataStorage",
                "test/DocumentFormat.OpenXml.Tests.Assets/assets/TestFiles",
                "test/DocumentFormat.OpenXml.Tests.Assets/assets/TestFilesValidation",
            ],
        )
        self.assertEqual(
            sources["phpspreadsheet-templates"]["path_scopes"],
            ["samples/templates"],
        )
        self.assertEqual(
            sources["xlsxwriter"]["path_scopes"],
            [
                "xlsxwriter/test/comparison/themes/technic.xlsx",
                "xlsxwriter/test/comparison/xlsx_files",
            ],
        )
        for source in sources.values():
            self.assertIn(source["commit"], source["source_url"])
            self.assertIn(source["commit"], source["declared_license_url"])
            self.assertRegex(source["declared_license_blob_sha1"], r"^[0-9a-f]{40}$")
            self.assertTrue(source["declared_license_path"])
            self.assertTrue(source["attribution"])
            self.assertTrue(source["default_provenance"]["evidence"])
            self.assertIsInstance(source["provenance_rules"], list)
            self.assertGreater(source["expected_files"], 0)
            self.assertGreater(source["expected_source_bytes"], 0)
        self.assertGreaterEqual(
            sum(source["expected_files"] for source in sources.values()),
            2_000,
        )
        self.assertEqual(recipe["rights_policy"]["active_content"]["tier"], "Q")
        self.assertEqual(recipe["rights_policy"]["embedded_media"]["tier"], "U")
        self.assertEqual(sources["phpspreadsheet-templates"]["rights_tier"], "U")

    def test_libreoffice_scope_is_exactly_internal_calc_workbooks(self) -> None:
        sources = {
            source["id"]: source for source in self.module.load_recipe(RECIPE)["sources"]
        }
        source = sources["libreoffice-calc-qa"]

        self.assertEqual(source["expected_files"], 1_538)
        self.assertEqual(source["expected_source_bytes"], 150_366_520)
        self.assertEqual(source["rights_tier"], "U")
        self.assertEqual(source["tree_traversal"], "scoped")
        self.assertEqual(
            source["include_extensions"],
            [".fods", ".ods", ".xls", ".xlsb", ".xlsm", ".xlsx"],
        )
        self.assertEqual(source["declared_license_path"], "COPYING.MPL")
        self.assertEqual(
            source["declared_license_blob_sha1"],
            "a612ad9813b006ce81d1ee438dd784da99a54007",
        )
        self.assertTrue(
            all(
                provenance["rights_tier"] in {"U", "Q"}
                for provenance in [source["default_provenance"]]
                + source["provenance_rules"]
            )
        )

    def test_scoped_tree_walk_avoids_truncated_repository_tree(self) -> None:
        source = dict(
            next(
                source
                for source in self.module.load_recipe(RECIPE)["sources"]
                if source["id"] == "libreoffice-calc-qa"
            )
        )
        source.pop("expected_files")
        source.pop("expected_source_bytes")
        trees = {
            source["commit"]: [
                {
                    "path": "COPYING.MPL",
                    "type": "blob",
                    "sha": source["declared_license_blob_sha1"],
                    "size": 16_725,
                },
                {"path": "sc", "type": "tree", "sha": "1" * 40},
            ],
            "1" * 40: [{"path": "qa", "type": "tree", "sha": "2" * 40}],
            "2" * 40: [{"path": "unit", "type": "tree", "sha": "3" * 40}],
            "3" * 40: [{"path": "data", "type": "tree", "sha": "4" * 40}],
            "4" * 40: [
                {"path": "book.fods", "type": "blob", "sha": "a" * 40, "size": 10},
                {"path": "xlsx/book.xlsx", "type": "blob", "sha": "b" * 40, "size": 20},
            ],
        }

        def fake_tree(repo: str, object_id: str, *, recursive: bool):
            self.assertEqual(repo, "LibreOffice/core")
            if object_id == "4" * 40:
                self.assertTrue(recursive)
            return trees[object_id]

        with mock.patch.object(self.module, "git_tree", side_effect=fake_tree):
            rows = self.module.discover_source(source, max_bytes=None)

        self.assertEqual(
            [row["source_path"] for row in rows],
            ["sc/qa/unit/data/book.fods", "sc/qa/unit/data/xlsx/book.xlsx"],
        )
        self.assertTrue(all(row["initial_rights_tier"] == "U" for row in rows))

    def test_discovery_stays_in_scope_and_preclassifies_macros(self) -> None:
        source = dict(
            next(
                source
                for source in self.module.load_recipe(RECIPE)["sources"]
                if source["id"] == "closedxml-examples"
            )
        )
        source.pop("expected_files")
        source.pop("expected_source_bytes")
        scope = source["path_scopes"][0]
        tree = [
            {
                "type": "blob",
                "path": source["declared_license_path"],
                "size": 1_000,
                "sha": source["declared_license_blob_sha1"],
            },
            {
                "type": "blob",
                "path": f"{scope}/safe.xlsx",
                "size": 10,
                "sha": "a" * 40,
            },
            {
                "type": "blob",
                "path": f"{scope}/macro.xlsm",
                "size": 11,
                "sha": "b" * 40,
            },
            {
                "type": "blob",
                "path": f"{scope}/too-large.xlsx",
                "size": 101,
                "sha": "c" * 40,
            },
            {
                "type": "blob",
                "path": "ClosedXML.Tests/Resource/Other/private.xlsx",
                "size": 9,
                "sha": "d" * 40,
            },
            {
                "type": "blob",
                "path": f"{scope}/image.png",
                "size": 8,
                "sha": "e" * 40,
            },
        ]

        with mock.patch.object(self.module, "source_tree", return_value=tree):
            rows = self.module.discover_source(source, max_bytes=100)

        self.assertEqual(
            [Path(row["source_path"]).name for row in rows],
            ["macro.xlsm", "safe.xlsx"],
        )
        self.assertEqual(rows[0]["initial_rights_tier"], "Q")
        self.assertEqual(rows[0]["risk_flags"], ["active_content_extension"])
        self.assertEqual(rows[1]["initial_rights_tier"], "S")
        self.assertEqual(rows[1]["provenance_class"], "project_authored_fixture")
        self.assertEqual(rows[1]["provenance_rule"], "default")

    def test_discovery_rejects_mismatched_license_blob_evidence(self) -> None:
        source = dict(
            next(
                source
                for source in self.module.load_recipe(RECIPE)["sources"]
                if source["id"] == "closedxml-examples"
            )
        )
        tree = [
            {
                "type": "blob",
                "path": source["declared_license_path"],
                "size": 1_000,
                "sha": "0" * 40,
            }
        ]

        with mock.patch.object(self.module, "source_tree", return_value=tree):
            with self.assertRaisesRegex(
                self.module.CorpusError, "pinned license blob mismatch"
            ):
                self.module.discover_source(source, max_bytes=100)

    def test_default_selection_excludes_active_content_before_download(self) -> None:
        source = next(
            source
            for source in self.module.load_recipe(RECIPE)["sources"]
            if source["id"] == "closedxml-examples"
        )
        path = source["path_scopes"][0] + "/macro.xlsm"
        tier, risks = self.module.initial_classification(source, path)
        row = {
            "extension": ".xlsm",
            "git_blob_sha1": "a" * 40,
            "initial_rights_tier": tier,
            "rights_tier": tier,
            "risk_flags": risks,
            "source_id": source["id"],
            "source_path": path,
            "source_size": 100,
            "source_url": "https://example.invalid/macro.xlsm",
        }

        fetch, excluded = self.module.prepare_rows([row], {"S"})

        self.assertEqual(fetch, [])
        self.assertEqual(excluded[0]["status"], "excluded")
        self.assertFalse(excluded[0]["eligible"])

    def test_explicit_q_acquisition_never_makes_payload_render_eligible(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            payload = dest / "payload/fixture/scope/macro.xlsm"
            write_package(payload, {"xl/vbaProject.bin": b"macro"})
            row = {
                "bytes": payload.stat().st_size,
                "extension": ".xlsm",
                "git_blob_sha1": self.module.git_blob_sha1(payload),
                "initial_rights_tier": "Q",
                "local_path": payload.relative_to(dest).as_posix(),
                "rights_tier": "Q",
                "risk_flags": ["active_content_extension"],
                "sha256": self.module.sha256_file(payload),
                "source_id": "fixture",
                "source_path": "scope/macro.xlsm",
                "source_size": payload.stat().st_size,
                "source_url": "https://example.invalid/macro.xlsm",
            }

            finalized = self.module.finalize_rows([row], [], dest, {"Q"})

            self.assertEqual(finalized[0]["status"], "quarantined")
            self.assertFalse(finalized[0]["eligible"])
            self.assertFalse(finalized[0]["render_selected"])
            self.assertNotIn("semantic_sha256", finalized[0])

    def test_file_level_provenance_never_infers_bug_attachment_rights(self) -> None:
        recipe = self.module.load_recipe(RECIPE)
        sources = {source["id"]: source for source in recipe["sources"]}
        poi = sources["apache-poi-spreadsheets"]
        calamine = sources["calamine-spreadsheets"]

        poi_bug = self.module.file_provenance(
            poi, "test-data/spreadsheet/123233_charts.xlsx"
        )
        poi_project = self.module.file_provenance(
            poi,
            "poi-examples/src/main/java/org/apache/poi/examples/ss/formula/"
            "mortgage-calculation.xls",
        )
        poi_danger = self.module.file_provenance(
            poi, "test-data/openxml4j/invalid.xlsx"
        )
        calamine_bug = self.module.file_provenance(
            calamine, "tests/issue_530.xlsx"
        )
        calamine_project = self.module.file_provenance(
            calamine, "tests/date.xlsx"
        )

        self.assertEqual(poi_bug["provenance_class"], "issue_or_bug_submission")
        self.assertEqual(poi_bug["provenance_rights_tier"], "U")
        self.assertEqual(
            poi_project["provenance_class"], "project_authored_fixture"
        )
        self.assertEqual(poi_danger["provenance_rights_tier"], "Q")
        self.assertEqual(
            calamine_bug["provenance_class"], "issue_or_bug_submission"
        )
        self.assertEqual(calamine_bug["provenance_rights_tier"], "U")
        self.assertEqual(
            calamine_project["provenance_class"], "project_authored_fixture"
        )
        self.assertEqual(calamine_project["provenance_rights_tier"], "S")


class RenderCorpusPackageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def test_package_scan_downgrades_media_external_and_active_content(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plain = root / "plain.xlsx"
            media = root / "media.xlsx"
            external = root / "external.xlsx"
            macro = root / "macro.xlsx"
            invalid = root / "invalid.xlsx"
            write_package(plain)
            write_package(media, {"xl/media/logo.png": b"png"})
            write_package(
                external,
                {
                    "xl/_rels/workbook.xml.rels": (
                        b'<Relationships><Relationship TargetMode="External" '
                        b'Target="https://example.invalid/data.xlsx"/></Relationships>'
                    )
                },
            )
            write_package(macro, {"xl/vbaProject.bin": b"macro"})
            invalid.write_bytes(b"not-a-zip")

            self.assertEqual(self.module.scan_package(plain, ".xlsx"), ("S", []))
            self.assertEqual(self.module.scan_package(media, ".xlsx"), ("U", ["embedded_media"]))
            self.assertEqual(
                self.module.scan_package(external, ".xlsx"),
                ("U", ["external_relationship"]),
            )
            self.assertEqual(
                self.module.scan_package(macro, ".xlsx"),
                ("Q", ["active_content_member"]),
            )
            self.assertEqual(
                self.module.scan_package(invalid, ".xlsx"),
                ("Q", ["invalid_zip_package"]),
            )

    def test_flat_odf_scan_is_bounded_and_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plain = root / "plain.fods"
            external = root / "external.fods"
            script = root / "script.fods"
            unsafe_declaration = root / "unsafe-declaration.fods"
            invalid = root / "invalid.fods"
            plain.write_bytes(b"<office:document xmlns:office='urn:office'/>")
            external.write_bytes(
                b"<office:document xmlns:office='urn:office' "
                b"xmlns:xlink='urn:xlink' xlink:href='https://example.invalid/book.ods'/>"
            )
            script.write_bytes(
                b"<office:document xmlns:office='urn:office'>"
                b"<office:scripts/></office:document>"
            )
            unsafe_declaration.write_bytes(
                b"<!DOCTYPE document [<!ENTITY payload 'blocked'>]>"
                b"<document>&payload;</document>"
            )
            invalid.write_bytes(b"<office:document")

            self.assertEqual(self.module.scan_package(plain, ".fods"), ("S", []))
            self.assertEqual(
                self.module.scan_package(external, ".fods"),
                ("U", ["external_relationship"]),
            )
            self.assertEqual(
                self.module.scan_package(script, ".fods"),
                ("Q", ["active_content_member"]),
            )
            self.assertEqual(
                self.module.scan_package(unsafe_declaration, ".fods"),
                ("Q", ["unsafe_xml_declaration"]),
            )
            self.assertEqual(
                self.module.scan_package(invalid, ".fods"),
                ("Q", ["invalid_flat_xml"]),
            )

    def test_flat_odf_semantic_hash_ignores_xml_whitespace(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = root / "first.fods"
            second = root / "second.fods"
            first.write_bytes(b"<document><table><cell>value</cell></table></document>")
            second.write_bytes(
                b"<document>\n <table>\n  <cell>value</cell>\n </table>\n</document>"
            )

            self.assertEqual(
                self.module.semantic_package_sha256(first, ".fods"),
                self.module.semantic_package_sha256(second, ".fods"),
            )

    def test_semantic_hash_ignores_zip_metadata_properties_and_calc_state(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = root / "first.xlsx"
            second = root / "second.xlsx"
            write_package(
                first,
                {
                    "docProps/core.xml": b"<core><created>one</created></core>",
                    "xl/calcChain.xml": b"<calcChain><c r='A1'/></calcChain>",
                    "xl/worksheets/sheet1.xml": (
                        b"<worksheet><sheetData/><calcPr calcId='1'/></worksheet>"
                    ),
                },
                timestamp=(2020, 1, 2, 3, 4, 6),
            )
            write_package(
                second,
                {
                    "docProps/core.xml": b"<core><created>two</created></core>",
                    "xl/calcChain.xml": b"<calcChain><c r='B2'/></calcChain>",
                    "xl/worksheets/sheet1.xml": (
                        b"<worksheet>\n <sheetData />\n <calcPr calcId='999'/>\n</worksheet>"
                    ),
                },
                timestamp=(2025, 6, 7, 8, 9, 10),
            )

            self.assertNotEqual(self.module.sha256_file(first), self.module.sha256_file(second))
            self.assertEqual(
                self.module.semantic_package_sha256(first, ".xlsx"),
                self.module.semantic_package_sha256(second, ".xlsx"),
            )

    def test_semantic_hash_retains_render_relevant_content(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = root / "first.xlsx"
            second = root / "second.xlsx"
            write_package(first, {"xl/worksheets/sheet1.xml": b"<sheet><v>1</v></sheet>"})
            write_package(second, {"xl/worksheets/sheet1.xml": b"<sheet><v>2</v></sheet>"})

            self.assertNotEqual(
                self.module.semantic_package_sha256(first, ".xlsx"),
                self.module.semantic_package_sha256(second, ".xlsx"),
            )

    def test_download_records_both_hashes_and_reuses_valid_cache(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source_file = root / "source.xlsx"
            write_package(source_file)
            size = source_file.stat().st_size
            row = {
                "extension": ".xlsx",
                "git_blob_sha1": self.module.git_blob_sha1(source_file),
                "initial_rights_tier": "S",
                "rights_tier": "S",
                "risk_flags": [],
                "source_id": "fixture",
                "source_path": "scope/book.xlsx",
                "source_size": size,
                "source_url": source_file.as_uri(),
            }
            dest = root / "dest"

            first = self.module.download_one(row, dest)
            second = self.module.download_one(row, dest)

            self.assertEqual(first, second)
            self.assertEqual(first["bytes"], size)
            self.assertEqual(first["sha256"], hashlib.sha256(source_file.read_bytes()).hexdigest())
            self.assertEqual(
                first["local_path"], "payload/fixture/scope/book.xlsx"
            )

    def test_exact_byte_dedup_uses_first_sorted_identity_as_canonical(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            first_path = dest / "payload/a/scope/a.xlsx"
            second_path = dest / "payload/b/scope/b.xlsx"
            write_package(first_path)
            second_path.parent.mkdir(parents=True)
            second_path.write_bytes(first_path.read_bytes())
            digest = hashlib.sha256(first_path.read_bytes()).hexdigest()
            blob = self.module.git_blob_sha1(first_path)
            common = {
                "bytes": first_path.stat().st_size,
                "extension": ".xlsx",
                "git_blob_sha1": blob,
                "initial_rights_tier": "S",
                "rights_tier": "S",
                "risk_flags": [],
                "sha256": digest,
                "source_size": first_path.stat().st_size,
                "source_url": "https://example.invalid/book.xlsx",
            }
            rows = [
                {
                    **common,
                    "local_path": "payload/b/scope/b.xlsx",
                    "source_id": "b",
                    "source_path": "scope/b.xlsx",
                },
                {
                    **common,
                    "local_path": "payload/a/scope/a.xlsx",
                    "source_id": "a",
                    "source_path": "scope/a.xlsx",
                },
            ]

            finalized = self.module.finalize_rows(rows, [], dest, {"S"})

            self.assertEqual(finalized[0]["status"], "ready")
            self.assertEqual(finalized[1]["status"], "duplicate")
            self.assertEqual(finalized[1]["duplicate_of"], "a:scope/a.xlsx")
            self.assertEqual(finalized[1]["local_path"], finalized[0]["local_path"])
            self.assertFalse(second_path.exists())

    def test_semantic_dedup_selects_first_sorted_package_for_rendering(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            first_path = dest / "payload/a/scope/a.xlsx"
            second_path = dest / "payload/b/scope/b.xlsx"
            write_package(first_path, timestamp=(2020, 1, 2, 3, 4, 6))
            write_package(
                second_path,
                {"docProps/core.xml": b"<core><created>later</created></core>"},
                timestamp=(2025, 6, 7, 8, 9, 10),
            )
            rows = []
            for source_id, source_path, local in (
                ("b", "scope/b.xlsx", second_path),
                ("a", "scope/a.xlsx", first_path),
            ):
                rows.append(
                    {
                        "bytes": local.stat().st_size,
                        "extension": ".xlsx",
                        "git_blob_sha1": self.module.git_blob_sha1(local),
                        "initial_rights_tier": "S",
                        "local_path": local.relative_to(dest).as_posix(),
                        "rights_tier": "S",
                        "risk_flags": [],
                        "sha256": self.module.sha256_file(local),
                        "source_id": source_id,
                        "source_path": source_path,
                        "source_size": local.stat().st_size,
                        "source_url": "https://example.invalid/book.xlsx",
                    }
                )

            finalized = self.module.finalize_rows(rows, [], dest, {"S"})

            self.assertTrue(finalized[0]["render_selected"])
            self.assertNotIn("semantic_duplicate_of", finalized[0])
            self.assertFalse(finalized[1]["render_selected"])
            self.assertEqual(finalized[1]["semantic_duplicate_of"], "a:scope/a.xlsx")


class RenderCorpusManifestTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_module()

    def _write_verified_manifest(self, root: Path) -> tuple[Path, Path]:
        recipe = self.module.load_recipe(RECIPE)
        source = next(
            source
            for source in recipe["sources"]
            if source["id"] == "closedxml-examples"
        )
        source_path = source["path_scopes"][0] + "/BasicWorkbook.xlsx"
        relative = Path("payload") / source["id"] / Path(source_path)
        payload = root / relative
        write_package(payload)
        tier, risks = self.module.initial_classification(source, source_path)
        downloaded = [
            {
                **self.module.source_rights_fields(source),
                **self.module.file_provenance(source, source_path),
                "bytes": payload.stat().st_size,
                "extension": ".xlsx",
                "git_blob_sha1": self.module.git_blob_sha1(payload),
                "initial_rights_tier": tier,
                "local_path": relative.as_posix(),
                "rights_tier": tier,
                "risk_flags": risks,
                "sha256": self.module.sha256_file(payload),
                "source_id": source["id"],
                "source_path": source_path,
                "source_size": payload.stat().st_size,
                "source_url": self.module.raw_url(source["repo"], source["commit"], source_path),
            }
        ]
        rows = self.module.finalize_rows(downloaded, [], root, {"S"})
        manifest = self.module.build_manifest(
            recipe, RECIPE, [source], rows, {"S"}, 25_000_000, 1
        )
        manifest_path = root / "manifest.json"
        self.module.write_manifest(manifest_path, manifest)
        return manifest_path, payload

    def test_manifest_is_deterministic_and_verifies_without_network(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest_path, _ = self._write_verified_manifest(root)
            first = manifest_path.read_bytes()
            manifest = json.loads(first)
            self.module.write_manifest(manifest_path, manifest)

            self.assertEqual(first, manifest_path.read_bytes())
            self.assertEqual(self.module.verify_manifest(manifest_path, RECIPE), [])
            row = manifest["files"][0]
            self.assertRegex(row["sha256"], r"^[0-9a-f]{64}$")
            self.assertRegex(row["git_blob_sha1"], r"^[0-9a-f]{40}$")

    def test_verify_rejects_payload_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path, payload = self._write_verified_manifest(Path(tmp))
            payload.write_bytes(payload.read_bytes() + b"tamper")

            errors = self.module.verify_manifest(manifest_path, RECIPE)

            self.assertTrue(any("mismatch" in error for error in errors), errors)

    def test_verify_rejects_unpinned_row_source_url(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path, _ = self._write_verified_manifest(Path(tmp))
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["files"][0]["source_url"] = "https://example.invalid/latest.xlsx"
            self.module.write_manifest(manifest_path, manifest)

            errors = self.module.verify_manifest(manifest_path, RECIPE)

            self.assertTrue(
                any(
                    error.startswith(
                        "source URL is not commit-pinned for closedxml-examples:"
                    )
                    for error in errors
                ),
                errors,
            )

    def test_verify_rejects_tampered_file_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path, _ = self._write_verified_manifest(Path(tmp))
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["files"][0]["provenance_class"] = "unreviewed_upstream_fixture"
            self.module.write_manifest(manifest_path, manifest)

            errors = self.module.verify_manifest(manifest_path, RECIPE)

            self.assertTrue(
                any(
                    error.startswith("provenance_class mismatch for closedxml-examples:")
                    for error in errors
                ),
                errors,
            )

    def test_verify_fails_closed_instead_of_crashing_on_malformed_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path, _ = self._write_verified_manifest(Path(tmp))
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["files"] = [{}]
            self.module.write_manifest(manifest_path, manifest)

            errors = self.module.verify_manifest(manifest_path, RECIPE)

            self.assertIn(
                "manifest files contain rows without string identities", errors
            )
            self.assertIn(
                "manifest summary cannot be derived from malformed file rows", errors
            )

    def test_dry_run_and_list_do_not_create_destination(self) -> None:
        recipe = self.module.load_recipe(RECIPE)
        source = next(
            source
            for source in recipe["sources"]
            if source["id"] == "closedxml-examples"
        )
        path = source["path_scopes"][0] + "/Book.xlsx"
        rows = [
            {
                "extension": ".xlsx",
                "git_blob_sha1": "a" * 40,
                "initial_rights_tier": "S",
                "rights_tier": "S",
                "risk_flags": [],
                "source_id": source["id"],
                "source_path": path,
                "source_size": 10,
                "source_url": "https://example.invalid/Book.xlsx",
            }
        ]
        with tempfile.TemporaryDirectory() as tmp:
            destination = Path(tmp) / "not-created"
            for action in ("--dry-run", "--list"):
                with self.subTest(action=action):
                    stdout = io.StringIO()
                    with mock.patch.object(self.module, "discover_all", return_value=rows):
                        with contextlib.redirect_stdout(stdout):
                            result = self.module.main(
                                [
                                    action,
                                    "--recipe",
                                    str(RECIPE),
                                    "--source",
                                    source["id"],
                                    "--dest",
                                    str(destination),
                                ]
                            )
                    self.assertEqual(result, 0)
                    self.assertFalse(destination.exists())
                    self.assertIn(source["id"], stdout.getvalue())


if __name__ == "__main__":
    unittest.main()
