#!/usr/bin/env python3
"""Tests for the ODS oracle harness."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
from zipfile import ZIP_STORED, ZipFile, ZipInfo


ROOT = Path(__file__).resolve().parents[1]
SCRIPT_DIR = ROOT / "scripts"


def load_ods_parity_module():
    sys.path.insert(0, str(SCRIPT_DIR))
    spec = importlib.util.spec_from_file_location(
        "ods_odfpy_parity", SCRIPT_DIR / "ods-odfpy-parity.py"
    )
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class OdsOracleTests(unittest.TestCase):
    def test_skip_classification_maps_rows_to_decisions_and_evidence(self) -> None:
        module = load_ods_parity_module()
        cases = [
            (
                "oracle-error",
                "RuntimeError: ODS oracle value limit exceeded",
                ("documented_bounded_oracle", "ods_repeated_value_limit", None),
            ),
            (
                "oracle-error",
                "ParseError: not well-formed (invalid token): line 1, column 1",
                ("needs_corpus_crosscheck", "ods_parse_error", None),
            ),
            (
                "oracle-timeout",
                "oracle timeout after 5s",
                ("needs_bounded_oracle", "ods_oracle_timeout", None),
            ),
            (
                "oracle-error",
                "ValueError: unknown ODS oracle failure",
                ("needs_oracle_triage", "ods_oracle_exception", None),
            ),
        ]

        for kind, reason, expected in cases:
            with self.subTest(kind=kind, reason=reason):
                self.assertEqual(module.skip_classification(kind, reason), expected)

    def test_show_skips_refines_parse_errors_from_corpus_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            ok_path = base / "ok.ods"
            write_ods(
                ok_path,
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Data">
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p>ok</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
""",
            )

            protected_path = base / "calamine" / "tests" / "pass_protected.ods"
            protected_path.parent.mkdir(parents=True)
            with ZipFile(protected_path, "w") as archive:
                archive.writestr("content.xml", b"\x04")

            manifest_path = base / "manifest.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "files": [
                            {
                                "source": "unit",
                                "path": "ok.ods",
                                "local_path": str(ok_path),
                                "status": "downloaded",
                            },
                            {
                                "source": "unit",
                                "path": "tests/pass_protected.ods",
                                "local_path": str(protected_path),
                                "status": "downloaded",
                            },
                        ]
                    }
                ),
                encoding="utf-8",
            )

            corpus_report = base / "corpus-report.txt"
            corpus_report.write_text(
                "failure: .ods tests/pass_protected.ods "
                "kind=unsupported_encrypted_opendocument decision=unsupported_encrypted "
                "evidence=encrypted_opendocument_package container=zip "
                "extension_mismatch=false parse: unsupported encrypted OpenDocument package\n",
                encoding="utf-8",
            )

            fake_extract = base / "fake-rxls-extract"
            fake_extract.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env python3
                    print("# Sheet1")
                    print("ok")
                    """
                ),
                encoding="utf-8",
            )
            os.chmod(fake_extract, 0o755)

            output = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT_DIR / "ods-odfpy-parity.py"),
                    "--manifest",
                    str(manifest_path),
                    "--bin",
                    str(fake_extract),
                    "--corpus-report",
                    str(corpus_report),
                    "--show-skips",
                    "5",
                    "--min",
                    "1.0",
                ],
                capture_output=True,
                text=True,
            )

            self.assertEqual(output.returncode, 0, output.stdout + output.stderr)
            self.assertIn("files: 2", output.stdout)
            self.assertIn("oracle-skipped: 1", output.stdout)
            self.assertIn("comparable: 1", output.stdout)
            self.assertIn(
                "by_skip_decision: unsupported_encrypted skipped=1",
                output.stdout,
            )
            self.assertIn(
                "by_skip_evidence: encrypted_opendocument_package skipped=1",
                output.stdout,
            )
            self.assertIn(
                "by_skip_corpus_kind: unsupported_encrypted_opendocument skipped=1",
                output.stdout,
            )
            self.assertRegex(
                output.stdout,
                r"skip: kind=oracle-error decision=unsupported_encrypted evidence=encrypted_opendocument_package corpus_kind=unsupported_encrypted_opendocument path=.*pass_protected\.ods reason=ParseError: .*",
            )

    def test_hostile_repeats_fail_fast_with_value_limit_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "hostile.ods"
            write_ods(
                path,
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Data">
        <table:table-row table:number-rows-repeated="999999999">
          <table:table-cell table:number-columns-repeated="999999999" office:value-type="string">
            <text:p>X</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
""",
            )

            module = load_ods_parity_module()
            result = module.run_with_timeout(
                module.visible_ods_values,
                (str(path),),
                timeout_seconds=1.0,
            )

        self.assertEqual(result.status, "error")
        self.assertEqual(result.error, "RuntimeError: ODS oracle value limit exceeded")

    def test_display_oracle_uses_visible_text_and_typed_bool_values(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "display.ods"
            write_ods(
                path,
                """<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Data">
        <table:table-row>
          <table:table-cell office:value-type="date" office:date-value="2021-01-01T10:10:10">
            <text:p>01/01/2021 10:10 AM</text:p>
          </table:table-cell>
          <table:table-cell office:value-type="float" office:value="0.5">
            <text:p>1</text:p>
          </table:table-cell>
          <table:table-cell office:value-type="boolean" office:boolean-value="false">
            <text:p>FAUX</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="string">
            <text:p>A<text:s text:c="2"/>B<text:span>c</text:span></text:p>
          </table:table-cell>
          <table:table-cell table:number-columns-repeated="2" office:value-type="string">
            <text:p>Repeat</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="string">
            <text:p>Cell</text:p>
            <office:annotation><text:p>Note text</text:p></office:annotation>
          </table:table-cell>
          <table:table-cell office:value-type="string">
            <text:p>Split</text:p><text:p>Line</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>
""",
            )

            module = load_ods_parity_module()

            self.assertEqual(
                module.visible_ods_values(str(path)),
                [
                    "01/01/2021 10:10 AM",
                    "1",
                    "false",
                    "A  Bc",
                    "Repeat",
                    "Repeat",
                    "Cell",
                    "SplitLine",
                ],
            )


def write_ods(path: Path, content_xml: str) -> None:
    with ZipFile(path, "w") as archive:
        mimetype = ZipInfo("mimetype")
        mimetype.compress_type = ZIP_STORED
        archive.writestr(mimetype, "application/vnd.oasis.opendocument.spreadsheet")
        archive.writestr("content.xml", content_xml)


if __name__ == "__main__":
    unittest.main()
