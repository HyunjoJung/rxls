#!/usr/bin/env python3
"""Select one immutable attempt-bound artifact from a GitHub workflow run."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


POSITIVE_INTEGER = re.compile(r"[1-9][0-9]*")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


class SelectionError(ValueError):
    """Raised when run artifacts do not prove one usable candidate."""


def _require(condition: bool, label: str) -> None:
    if not condition:
        raise SelectionError(label)


def select_artifact(
    payload: Any, *, name_prefix: str, current_attempt: int
) -> dict[str, int | str]:
    _require(isinstance(payload, dict), "payload")
    artifacts = payload.get("artifacts")
    _require(isinstance(artifacts, list), "artifacts")
    _require(payload.get("total_count") == len(artifacts), "artifacts.pagination")
    _require(current_attempt > 0, "current_attempt")
    _require(
        bool(name_prefix)
        and "\n" not in name_prefix
        and "\r" not in name_prefix,
        "name_prefix",
    )

    candidates: dict[int, dict[str, int | str]] = {}
    for index, artifact in enumerate(artifacts):
        _require(isinstance(artifact, dict), f"artifacts[{index}]")
        name = artifact.get("name")
        if not isinstance(name, str) or not name.startswith(name_prefix):
            continue
        suffix = name.removeprefix(name_prefix)
        _require(POSITIVE_INTEGER.fullmatch(suffix) is not None, f"artifact.name:{name}")
        attempt = int(suffix)
        _require(attempt <= current_attempt, f"artifact.future_attempt:{name}")
        artifact_id = artifact.get("id")
        size = artifact.get("size_in_bytes")
        digest = artifact.get("digest")
        _require(
            isinstance(artifact_id, int) and artifact_id > 0,
            f"artifact.id:{name}",
        )
        _require(isinstance(size, int) and size > 0, f"artifact.size:{name}")
        _require(artifact.get("expired") is False, f"artifact.expired:{name}")
        _require(
            isinstance(digest, str) and DIGEST.fullmatch(digest) is not None,
            f"artifact.digest:{name}",
        )
        _require(attempt not in candidates, f"artifact.duplicate_attempt:{attempt}")
        candidates[attempt] = {
            "artifact_id": artifact_id,
            "artifact_name": name,
            "artifact_digest": digest,
            "source_attempt": attempt,
        }

    _require(bool(candidates), "artifact.missing")
    selected_attempt = current_attempt if current_attempt in candidates else max(candidates)
    return candidates[selected_attempt]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--name-prefix", required=True)
    parser.add_argument("--current-attempt", type=int, required=True)
    parser.add_argument("--github-output", type=Path, required=True)
    args = parser.parse_args()
    try:
        selected = select_artifact(
            json.loads(args.artifacts.read_text(encoding="utf-8")),
            name_prefix=args.name_prefix,
            current_attempt=args.current_attempt,
        )
        with args.github_output.open("a", encoding="utf-8", newline="\n") as output:
            for key in (
                "artifact_id",
                "artifact_name",
                "artifact_digest",
                "source_attempt",
            ):
                output.write(f"{key}={selected[key]}\n")
    except (OSError, json.JSONDecodeError, SelectionError) as error:
        raise SystemExit(f"artifact selection failed: {error}") from error
    print(
        "selected verified artifact: "
        f"{selected['artifact_name']} (id {selected['artifact_id']})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
