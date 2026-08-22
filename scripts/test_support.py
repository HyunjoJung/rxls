"""Cross-platform helpers shared by script tests."""

from __future__ import annotations

import os
from pathlib import Path
import sys


def make_python_stub_executable(script: Path) -> Path:
    """Return an executable launcher for a Python stub written at `script`."""
    os.chmod(script, 0o755)
    if os.name != "nt":
        return script

    launcher = script.with_suffix(".cmd")
    launcher.write_text(
        f'@echo off\n"{sys.executable}" "{script}" %*\n',
        encoding="utf-8",
    )
    return launcher
