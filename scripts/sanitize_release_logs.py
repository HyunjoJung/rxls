#!/usr/bin/env python3
"""Remove machine-local paths and generated fuzz payloads from release logs."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


ISO_TIMESTAMP = re.compile(
    r"\b\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:[.]\d+)?\b"
)
SOFFICE_PROCESS = re.compile(r"\bsoffice\[\d+(?::\d+)?\]")
FUZZ_DICTIONARY_BLOCK = re.compile(
    r"^###### Recommended dictionary[.] ######\r?\n"
    r".*?"
    r"^###### End of recommended dictionary[.] ######\r?\n?",
    re.MULTILINE | re.DOTALL,
)
FUZZ_DE_PAYLOAD = re.compile(r"(?m)(\sDE: ).*$")


def sanitize_text(text: str, workspace: Path, home: Path) -> str:
    """Replace local paths and irrelevant generated payloads with markers."""
    replacements: dict[str, str] = {}
    for path, marker in ((workspace, "<workspace>"), (home, "<home>")):
        expanded = path.expanduser()
        for spelling in (str(expanded.absolute()), str(expanded.resolve())):
            replacements[spelling] = marker
    for source, replacement in sorted(
        replacements.items(), key=lambda item: len(item[0]), reverse=True
    ):
        text = text.replace(source, replacement)
        text = text.replace(source.replace("/", "\\"), replacement)
    text = ISO_TIMESTAMP.sub("<timestamp>", text)
    text = SOFFICE_PROCESS.sub("soffice[<process>]", text)
    text = FUZZ_DICTIONARY_BLOCK.sub(
        "###### Generated fuzz dictionary redacted. ######\n", text
    )
    text = FUZZ_DE_PAYLOAD.sub(r"\1<generated-input>-", text)
    return text


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="+", type=Path)
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--home", type=Path, default=Path.home())
    args = parser.parse_args()

    for path in args.paths:
        text = path.read_text(encoding="utf-8", errors="replace")
        sanitized = sanitize_text(text, args.workspace, args.home)
        path.write_text(sanitized, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
