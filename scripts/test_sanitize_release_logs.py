#!/usr/bin/env python3
"""Tests for deterministic release-log path sanitation."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("sanitize_release_logs.py")


def _load():
    spec = importlib.util.spec_from_file_location("sanitize_release_logs", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SanitizeReleaseLogsTests(unittest.TestCase):
    def test_workspace_and_home_paths_become_stable_markers(self) -> None:
        module = _load()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp) / "user"
            workspace = home / "work" / "rxls"
            text = f"build {workspace}/fuzz\ncache {home}/.cargo\n"
            self.assertEqual(
                module.sanitize_text(text, workspace, home),
                "build <workspace>/fuzz\ncache <home>/.cargo\n",
            )

    def test_libreoffice_process_and_timestamp_fields_are_stable(self) -> None:
        module = _load()
        text = (
            "2026-07-15 20:21:25.969 soffice[9980:4290657] "
            "Task policy set failed\n"
        )

        self.assertEqual(
            module.sanitize_text(text, Path("/workspace"), Path("/home/runner")),
            "<timestamp> soffice[<process>] Task policy set failed\n",
        )

    def test_generated_fuzz_dictionary_payloads_are_redacted(self) -> None:
        module = _load()
        generated_drive = "r" + ":/"
        windows_home = "C" + ":/Us" + "ers/joe/reproducer"
        text = (
            f'#127493 NEW cov: 1 DE: "{generated_drive}\\000"-"safe"-\n'
            "###### Recommended dictionary. ######\n"
            f'"{generated_drive}\\000" # Uses: 4\n'
            "###### End of recommended dictionary. ######\n"
            "Done 140610 runs in 121 second(s)\n"
            f"artifact remains at {windows_home}\n"
        )

        self.assertEqual(
            module.sanitize_text(text, Path("/workspace"), Path("/home/runner")),
            "#127493 NEW cov: 1 DE: <generated-input>-\n"
            "###### Generated fuzz dictionary redacted. ######\n"
            "Done 140610 runs in 121 second(s)\n"
            f"artifact remains at {windows_home}\n",
        )


if __name__ == "__main__":
    unittest.main()
