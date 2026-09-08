#!/usr/bin/env python3
"""Authenticate and verify a core release handoff, without publication authority.

Both the read-only hosted rehearsal and tag publication use these checks. This
helper never dispatches, tags, publishes, or accesses registry credentials.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
MAX_METADATA_BYTES = 64 * 1024
MAX_CRATE_BYTES = 1 << 20
COMMAND_TIMEOUT = 1200
SEMVER = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?")


class HandoffError(ValueError):
    """Fail-closed artifact or package identity failure."""


def require(condition, message):
    if not condition:
        raise HandoffError(message)


def read_bounded(path, limit):
    require(not path.is_symlink() and path.is_file(), "expected a regular evidence file")
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    require(len(data) <= limit, "evidence file exceeds its byte budget")
    return data


def load_metadata(path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "duplicate artifact metadata key")
            result[key] = value
        return result
    return json.loads(read_bounded(path, MAX_METADATA_BYTES), object_pairs_hook=unique)


def positive(env, name):
    value = env.get(name, "")
    require(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,18}", value), f"invalid {name}")
    return int(value)


def identity(env):
    version, sha = env.get("VERIFIED_VERSION", ""), env.get("GITHUB_SHA", "")
    require(isinstance(version, str) and len(version) <= 128 and SEMVER.fullmatch(version), "invalid verified version")
    require(isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{40}", sha), "invalid source SHA")
    require(env.get("GITHUB_REPOSITORY") == "HyunjoJung/rxls", "foreign source repository")
    return version, sha


def authenticate(artifact, env):
    version, sha = identity(env)
    digest = env.get("ARTIFACT_DIGEST", "")
    require(isinstance(digest, str) and len(digest) <= 71, "invalid artifact digest input")
    digest = digest.removeprefix("sha256:")
    require(re.fullmatch(r"[0-9a-f]{64}", digest), "invalid verified artifact digest")
    attempt = positive(env, "SOURCE_ATTEMPT")
    require(attempt <= positive(env, "GITHUB_RUN_ATTEMPT"), "invalid verified source attempt")
    run_id, repository_id = positive(env, "GITHUB_RUN_ID"), positive(env, "GITHUB_REPOSITORY_ID")
    expected = {
        "id": positive(env, "ARTIFACT_ID"),
        "name": f"rxls-{version}-publication-{sha}-{run_id}-{attempt}",
        "digest": f"sha256:{digest}",
        "expired": False,
    }
    require(isinstance(artifact, dict), "invalid publication artifact metadata")
    require(all(type(artifact.get(key)) is type(value) and artifact[key] == value for key, value in expected.items()), "artifact identity differs from verified output")
    run = artifact.get("workflow_run")
    expected_run = {
        "id": run_id, "head_sha": sha,
        "repository_id": repository_id, "head_repository_id": repository_id,
    }
    require(isinstance(run, dict) and all(type(run.get(key)) is type(value) and run[key] == value for key, value in expected_run.items()), "artifact source differs from verified run")
    return {"artifact_id": expected["id"], "artifact_digest": expected["digest"], "source_attempt": attempt}


def checked_command(argv, root):
    subprocess.run(argv, cwd=root, check=True, timeout=COMMAND_TIMEOUT)


def verify(root, env, runner=checked_command):
    """Validate transferred evidence and compare a newly built archive exactly."""
    version, sha = identity(env)
    artifact = authenticate(load_metadata(root / "target/publication/artifact.json"), env)
    manifest = tomllib.loads(read_bounded(root / "Cargo.toml", MAX_METADATA_BYTES).decode("utf-8"))
    require(manifest["package"]["version"] == version, "source manifest version differs")
    commands = (
        ["python3", "scripts/check_workflow_policy.py"],
        ["python3", "scripts/check_release_identity.py"],
        ["python3", "scripts/check_cargo_publish_dry_run.py", "verify", "--manifest", "Cargo.toml", "--git-sha", sha, "--receipt", "dist/release-cargo-publish-dry-run.json"],
        ["python3", "scripts/release_manifest.py", "--verify-bundle", "dist", "--expected-files", "52", "--version", version, "--git-rev", sha],
        ["python3", "scripts/check_core_package.py", f"dist/rxls-{version}.crate"],
        ["cargo", "package", "--locked"],
    )
    for argv in commands:
        runner(argv, root)
    # Cargo has no stable publish-existing-archive command. The same clean,
    # pinned source/toolchain must reproduce the transferred archive bytewise.
    expected = read_bounded(root / f"dist/rxls-{version}.crate", MAX_CRATE_BYTES)
    actual = read_bounded(root / f"target/package/rxls-{version}.crate", MAX_CRATE_BYTES)
    require(actual == expected, "repacked crate differs from verified artifact bytes")
    return {
        "schema": "rxls.core-release-handoff.v1", "passed": True,
        "publication_allowed": False, "version": version, "git_rev": sha,
        "crate_bytes": len(actual), "crate_sha256": hashlib.sha256(actual).hexdigest(),
        **artifact,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("authenticate", "verify"))
    args = parser.parse_args(argv)
    try:
        if args.operation == "authenticate":
            result = authenticate(load_metadata(ROOT / "target/publication/artifact.json"), os.environ)
        else:
            result = verify(ROOT, os.environ)
            receipt = ROOT / "target/publication/handoff.json"
            require(not receipt.is_symlink(), "handoff receipt must not be a symlink")
            receipt.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result, sort_keys=True))
        return 0
    except (HandoffError, OSError, ValueError, KeyError, TypeError, RecursionError, subprocess.SubprocessError) as error:
        print(f"core release handoff failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
