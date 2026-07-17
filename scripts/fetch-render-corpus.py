#!/usr/bin/env python3
"""Acquire a pinned, license-aware corpus for spreadsheet render parity.

The payload is written only below ``local/render-corpus`` (or an explicit
destination) and must never be committed. The checked-in source recipe records
the exact upstream commits, licensed path scopes, attribution, and conservative
rights policy. The generated local manifest adds Git blob IDs, SHA-256 hashes,
package-risk evidence, and deterministic exact-byte deduplication.

Rights tiers are deliberately fail-closed:

* S: shareable, licensed fixture scope with no detected package risk.
* U: internal-only pending media/external-reference provenance review.
* Q: quarantine for active, encrypted, malformed, or opaque content.

By default only initial S-tier candidates are fetched. A downloaded package
that reveals media or active content is retained locally but downgraded to U or
Q and marked ``quarantined`` rather than ``ready``. Use ``--include-tier`` only
in an isolated review lane; selecting Q never executes a workbook.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
from http.client import HTTPException
import json
import os
from pathlib import Path, PurePosixPath
import re
import sys
import tempfile
import time
from typing import Iterable
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen
import zipfile
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RECIPE = ROOT / "scripts" / "render-corpus-sources.json"
DEFAULT_DEST = ROOT / "local" / "render-corpus"
DEFAULT_MAX_BYTES = 25_000_000
MANIFEST_SCHEMA = "rxls.render-corpus-manifest.v1"
RECIPE_SCHEMA = "rxls.render-corpus-sources.v1"
USER_AGENT = "rxls-render-corpus-fetcher/1"
RIGHTS_ORDER = {"S": 0, "U": 1, "Q": 2}
PROVENANCE_CLASSES = {
    "issue_or_bug_submission",
    "project_authored_fixture",
    "third_party_or_unreviewed_fixture",
    "unreviewed_upstream_fixture",
}
SPREADSHEET_EXTENSIONS = {".fods", ".xls", ".xlsx", ".xlsm", ".xlsb", ".ods"}
READY_STATUSES = {"ready", "quarantined", "duplicate"}
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
EXTERNAL_RELATIONSHIP = re.compile(
    br"targetmode\s*=\s*['\"]external['\"]", re.IGNORECASE
)
MAX_PACKAGE_MEMBER_BYTES = 64 * 1024 * 1024
MAX_PACKAGE_UNCOMPRESSED_BYTES = 256 * 1024 * 1024
SEMANTIC_IGNORED_MEMBERS = {
    "meta.xml",
    "xl/calcchain.xml",
}


class CorpusError(RuntimeError):
    """A stable, user-actionable corpus acquisition error."""


def canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_blob_sha1(path: Path) -> str:
    digest = hashlib.sha1()  # GitHub's tree API exposes SHA-1 Git object IDs.
    digest.update(f"blob {path.stat().st_size}\0".encode("ascii"))
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _semantic_xml_bytes(payload: bytes) -> bytes:
    try:
        root = ET.fromstring(payload)
    except ET.ParseError:
        return payload
    for parent in root.iter():
        for child in list(parent):
            if child.tag.rsplit("}", 1)[-1].lower() == "calcpr":
                parent.remove(child)
        if parent.text is not None and not parent.text.strip():
            parent.text = None
        if parent.tail is not None and not parent.tail.strip():
            parent.tail = None
    serialized = ET.tostring(root, encoding="unicode")
    try:
        canonical = ET.canonicalize(
            xml_data=serialized,
            with_comments=False,
            strip_text=False,
            rewrite_prefixes=True,
        )
    except (ET.ParseError, ValueError):
        return serialized.encode("utf-8")
    return canonical.encode("utf-8")


def semantic_package_sha256(path: Path, extension: str) -> str:
    """Hash render-relevant package members independent of ZIP metadata."""
    if extension == ".xls":
        return sha256_file(path)
    if extension == ".fods":
        if path.stat().st_size > MAX_PACKAGE_UNCOMPRESSED_BYTES:
            raise CorpusError("flat ODF exceeds semantic-hash expansion limits")
        payload = path.read_bytes()
        canonical = _semantic_xml_bytes(payload)
        digest = hashlib.sha256()
        digest.update(b"rxls-render-flat-odf-semantic-v1\0")
        digest.update(len(canonical).to_bytes(8, "little"))
        digest.update(canonical)
        return digest.hexdigest()
    digest = hashlib.sha256()
    digest.update(b"rxls-render-package-semantic-v1\0")
    with zipfile.ZipFile(path) as archive:
        infos = sorted(
            archive.infolist(),
            key=lambda info: (info.filename.replace("\\", "/").lower(), info.filename),
        )
        total = sum(info.file_size for info in infos)
        if (
            any(info.file_size > MAX_PACKAGE_MEMBER_BYTES for info in infos)
            or total > MAX_PACKAGE_UNCOMPRESSED_BYTES
        ):
            raise CorpusError("package exceeds semantic-hash expansion limits")
        occurrences: dict[str, int] = {}
        for info in infos:
            name = info.filename.replace("\\", "/").lstrip("/").lower()
            if name.startswith("docprops/") or name in SEMANTIC_IGNORED_MEMBERS:
                continue
            occurrence = occurrences.get(name, 0)
            occurrences[name] = occurrence + 1
            with archive.open(info, "r") as member:
                payload = member.read(MAX_PACKAGE_MEMBER_BYTES + 1)
            if len(payload) > MAX_PACKAGE_MEMBER_BYTES:
                raise CorpusError("package member exceeds semantic-hash limit")
            if name.endswith((".xml", ".rels")) or name in {
                "[content_types].xml",
                "content.xml",
                "settings.xml",
                "styles.xml",
            }:
                payload = _semantic_xml_bytes(payload)
            record = f"{name}#{occurrence}".encode("utf-8")
            digest.update(len(record).to_bytes(8, "little"))
            digest.update(record)
            digest.update(len(payload).to_bytes(8, "little"))
            digest.update(payload)
    return digest.hexdigest()


def is_safe_repo_path(value: str) -> bool:
    if not value or "\0" in value or "\\" in value:
        return False
    path = PurePosixPath(value)
    return (
        not path.is_absolute()
        and ".." not in path.parts
        and value == path.as_posix()
    )


def validate_source(source: dict) -> None:
    required = {
        "id",
        "repo",
        "commit",
        "path_scopes",
        "provenance_rules",
        "include_extensions",
        "declared_license",
        "declared_license_blob_sha1",
        "declared_license_path",
        "declared_license_url",
        "default_provenance",
        "expected_files",
        "expected_source_bytes",
        "rights_tier",
        "attribution",
        "source_url",
        "scope_basis",
    }
    missing = sorted(required - source.keys())
    if missing:
        raise CorpusError(f"source is missing required fields: {missing}")
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", str(source["id"])):
        raise CorpusError(f"invalid source id: {source['id']!r}")
    if not re.fullmatch(r"[^/\s]+/[^/\s]+", str(source["repo"])):
        raise CorpusError(f"invalid GitHub repository: {source['repo']!r}")
    if not HEX40.fullmatch(str(source["commit"])):
        raise CorpusError(f"source {source['id']} does not use a full commit SHA")
    if source["rights_tier"] not in RIGHTS_ORDER:
        raise CorpusError(f"source {source['id']} has an invalid rights tier")
    if source.get("tree_traversal", "recursive") not in {"recursive", "scoped"}:
        raise CorpusError(f"source {source['id']} has an invalid tree traversal")
    if not isinstance(source["expected_files"], int) or source["expected_files"] < 1:
        raise CorpusError(f"source {source['id']} has an invalid expected file count")
    if (
        not isinstance(source["expected_source_bytes"], int)
        or source["expected_source_bytes"] < 1
    ):
        raise CorpusError(f"source {source['id']} has an invalid expected byte count")
    scopes = source["path_scopes"]
    if not isinstance(scopes, list) or not scopes:
        raise CorpusError(f"source {source['id']} has no path scope")
    if any(not is_safe_repo_path(str(scope)) for scope in scopes):
        raise CorpusError(f"source {source['id']} has an unsafe path scope")
    extensions = source["include_extensions"]
    if not isinstance(extensions, list) or not extensions:
        raise CorpusError(f"source {source['id']} has no extensions")
    normalized = [str(ext).lower() for ext in extensions]
    if normalized != sorted(set(normalized)):
        raise CorpusError(f"source {source['id']} extensions must be sorted and unique")
    if any(ext not in SPREADSHEET_EXTENSIONS for ext in normalized):
        raise CorpusError(f"source {source['id']} includes an unsupported extension")
    license_path = str(source["declared_license_path"])
    if not is_safe_repo_path(license_path):
        raise CorpusError(f"source {source['id']} has an unsafe license path")
    expected_license_url = (
        f"https://github.com/{source['repo']}/blob/{source['commit']}/{license_path}"
    )
    if source["declared_license_url"] != expected_license_url:
        raise CorpusError(
            f"source {source['id']} license URL does not match its pinned license path"
        )
    if source["commit"] not in source["source_url"]:
        raise CorpusError(f"source {source['id']} source URL is not commit-pinned")
    if not HEX40.fullmatch(str(source["declared_license_blob_sha1"])):
        raise CorpusError(f"source {source['id']} has an invalid license blob ID")
    validate_provenance(source["id"], "default", source["default_provenance"])
    rules = source["provenance_rules"]
    if not isinstance(rules, list):
        raise CorpusError(f"source {source['id']} provenance rules must be a list")
    rule_ids: list[str] = []
    for rule in rules:
        if not isinstance(rule, dict):
            raise CorpusError(f"source {source['id']} has a malformed provenance rule")
        rule_id = rule.get("id")
        pattern = rule.get("path_regex")
        if not isinstance(rule_id, str) or not re.fullmatch(
            r"[a-z0-9][a-z0-9-]*", rule_id
        ):
            raise CorpusError(f"source {source['id']} has an invalid provenance rule id")
        if not isinstance(pattern, str) or not pattern:
            raise CorpusError(
                f"source {source['id']} provenance rule {rule_id} has no path regex"
            )
        try:
            re.compile(pattern)
        except re.error as exc:
            raise CorpusError(
                f"source {source['id']} provenance rule {rule_id} has an invalid regex"
            ) from exc
        validate_provenance(source["id"], rule_id, rule)
        rule_ids.append(rule_id)
    if rule_ids != sorted(set(rule_ids)):
        raise CorpusError(
            f"source {source['id']} provenance rule ids must be sorted and unique"
        )


def validate_provenance(source_id: str, rule_id: str, provenance: object) -> None:
    if not isinstance(provenance, dict):
        raise CorpusError(f"source {source_id} provenance {rule_id} must be an object")
    if provenance.get("class") not in PROVENANCE_CLASSES:
        raise CorpusError(
            f"source {source_id} provenance {rule_id} has an invalid class"
        )
    if provenance.get("rights_tier") not in RIGHTS_ORDER:
        raise CorpusError(
            f"source {source_id} provenance {rule_id} has an invalid rights tier"
        )
    basis = provenance.get("basis")
    if not isinstance(basis, str) or not basis.strip():
        raise CorpusError(f"source {source_id} provenance {rule_id} has no basis")
    evidence = provenance.get("evidence")
    if (
        not isinstance(evidence, list)
        or not evidence
        or any(
            not isinstance(item, str) or not item.startswith("https://")
            for item in evidence
        )
        or evidence != sorted(set(evidence))
    ):
        raise CorpusError(
            f"source {source_id} provenance {rule_id} evidence must be sorted HTTPS URLs"
        )


def load_recipe(path: Path = DEFAULT_RECIPE) -> dict:
    try:
        recipe = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise CorpusError(f"cannot read render corpus recipe: {exc}") from exc
    if not isinstance(recipe, dict) or recipe.get("schema") != RECIPE_SCHEMA:
        raise CorpusError(f"expected source recipe schema {RECIPE_SCHEMA}")
    if set(recipe.get("rights_tiers", {})) != set(RIGHTS_ORDER):
        raise CorpusError("source recipe must define S, U, and Q rights tiers")
    if set(recipe.get("provenance_classes", {})) != PROVENANCE_CLASSES:
        raise CorpusError("source recipe must define every file provenance class")
    if set(recipe.get("rights_policy", {})) != {
        "active_content",
        "embedded_media",
        "external_relationship",
        "invalid_or_encrypted_package",
    }:
        raise CorpusError("source recipe has an incomplete rights policy")
    for name, policy in recipe["rights_policy"].items():
        if policy.get("tier") not in RIGHTS_ORDER or not policy.get("handling"):
            raise CorpusError(f"invalid rights policy: {name}")
    default_tiers = recipe.get("default_include_tiers")
    if (
        not isinstance(default_tiers, list)
        or any(not isinstance(tier, str) for tier in default_tiers)
        or default_tiers != sorted(set(default_tiers))
    ):
        raise CorpusError("default rights tiers must be sorted and unique")
    if any(tier not in RIGHTS_ORDER for tier in default_tiers):
        raise CorpusError("default rights tiers contain an unknown tier")
    sources = recipe.get("sources")
    if not isinstance(sources, list) or not sources:
        raise CorpusError("source recipe has no sources")
    ids: list[str] = []
    for source in sources:
        if not isinstance(source, dict):
            raise CorpusError("source recipe entries must be objects")
        validate_source(source)
        ids.append(source["id"])
    if ids != sorted(set(ids)):
        raise CorpusError("source ids must be sorted and unique")
    return recipe


def request_json(url: str) -> object:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": USER_AGENT,
        "X-GitHub-Api-Version": "2022-11-28",
    }
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = Request(url, headers=headers)
    try:
        with urlopen(request, timeout=60) as response:
            return json.load(response)
    except (HTTPError, URLError, HTTPException, OSError, json.JSONDecodeError) as exc:
        raise CorpusError(f"GitHub API request failed for {url}: {exc}") from exc


def git_tree(repo: str, object_id: str, *, recursive: bool) -> list[dict]:
    suffix = "?recursive=1" if recursive else ""
    url = f"https://api.github.com/repos/{repo}/git/trees/{quote(object_id)}{suffix}"
    payload = request_json(url)
    if not isinstance(payload, dict) or not isinstance(payload.get("tree"), list):
        raise CorpusError(f"unexpected GitHub tree response for {repo}@{object_id}")
    if payload.get("truncated"):
        raise CorpusError(f"GitHub returned a truncated tree for {repo}@{object_id}")
    return payload["tree"]


def source_tree(repo: str, commit: str) -> list[dict]:
    return git_tree(repo, commit, recursive=True)


def scoped_source_tree(source: dict) -> list[dict]:
    """Discover only pinned scopes when a repository-wide tree is too large.

    GitHub truncates recursive trees for very large repositories. Walking each
    path component non-recursively and requesting a recursive tree only at the
    terminal scope retains Git blob IDs and sizes without accepting a partial
    repository response.
    """
    repo = source["repo"]
    root = git_tree(repo, source["commit"], recursive=False)
    by_path: dict[str, dict] = {}
    listings: dict[str, list[dict]] = {source["commit"]: root}

    def listing(tree_id: str) -> list[dict]:
        if tree_id not in listings:
            listings[tree_id] = git_tree(repo, tree_id, recursive=False)
        return listings[tree_id]

    def resolve(path: str, *, recursive_terminal: bool) -> None:
        parts = PurePosixPath(path).parts
        tree_id = source["commit"]
        for index, part in enumerate(parts):
            matches = [item for item in listing(tree_id) if item.get("path") == part]
            if len(matches) != 1:
                raise CorpusError(
                    f"scoped tree path is missing or ambiguous for {source['id']}: {path}"
                )
            item = matches[0]
            terminal = index == len(parts) - 1
            prefix = "/".join(parts[: index + 1])
            if terminal:
                if item.get("type") == "blob":
                    row = dict(item)
                    row["path"] = prefix
                    by_path[prefix] = row
                    return
                if item.get("type") != "tree" or not isinstance(item.get("sha"), str):
                    raise CorpusError(
                        f"scoped tree terminal is not a tree or blob for {source['id']}: {path}"
                    )
                terminal_rows = git_tree(
                    repo, item["sha"], recursive=recursive_terminal
                )
                for child in terminal_rows:
                    child_path = child.get("path")
                    if not isinstance(child_path, str):
                        raise CorpusError(
                            f"scoped tree contains a malformed path for {source['id']}: {path}"
                        )
                    row = dict(child)
                    row["path"] = f"{prefix}/{child_path}"
                    by_path[row["path"]] = row
                return
            if item.get("type") != "tree" or not isinstance(item.get("sha"), str):
                raise CorpusError(
                    f"scoped tree component is not a tree for {source['id']}: {prefix}"
                )
            tree_id = item["sha"]

    resolve(source["declared_license_path"], recursive_terminal=False)
    for scope in source["path_scopes"]:
        resolve(scope, recursive_terminal=True)
    return [by_path[path] for path in sorted(by_path)]


def verify_license_tree_evidence(source: dict, tree: list[dict]) -> None:
    """Require the pinned tree to contain the exact declared license blob."""
    license_path = source["declared_license_path"]
    matches = [item for item in tree if item.get("path") == license_path]
    if len(matches) != 1 or matches[0].get("type") != "blob":
        raise CorpusError(
            f"pinned license path is missing or ambiguous for {source['id']}: "
            f"{license_path}"
        )
    actual = matches[0].get("sha")
    if actual != source["declared_license_blob_sha1"]:
        raise CorpusError(
            f"pinned license blob mismatch for {source['id']}: "
            f"expected {source['declared_license_blob_sha1']}, discovered {actual}"
        )


def raw_url(repo: str, commit: str, path: str) -> str:
    encoded_path = "/".join(quote(part, safe="") for part in path.split("/"))
    return f"https://raw.githubusercontent.com/{repo}/{commit}/{encoded_path}"


def path_is_scoped(path: str, scopes: Iterable[str]) -> bool:
    return any(path == scope or path.startswith(scope.rstrip("/") + "/") for scope in scopes)


def file_provenance(source: dict, path: str) -> dict[str, object]:
    selected = source["default_provenance"]
    rule_id = "default"
    for rule in source["provenance_rules"]:
        if re.search(rule["path_regex"], path):
            selected = rule
            rule_id = rule["id"]
            break
    return {
        "provenance_basis": selected["basis"],
        "provenance_class": selected["class"],
        "provenance_evidence": list(selected["evidence"]),
        "provenance_rule": rule_id,
        "provenance_rights_tier": selected["rights_tier"],
    }


def initial_classification(source: dict, path: str) -> tuple[str, list[str]]:
    provenance = file_provenance(source, path)
    tier = max_rights(
        source["rights_tier"], str(provenance["provenance_rights_tier"])
    )
    extension = PurePosixPath(path).suffix.lower()
    risks: list[str] = []
    if extension in {".xlsm", ".xlam", ".xltm"}:
        tier = max_rights(tier, "Q")
        risks.append("active_content_extension")
    elif extension in {".xls", ".xlsb"}:
        tier = max_rights(tier, "U")
        risks.append("opaque_binary_container")
    return tier, risks


def source_rights_fields(source: dict) -> dict[str, str]:
    return {
        "attribution": source["attribution"],
        "declared_license": source["declared_license"],
        "declared_license_blob_sha1": source["declared_license_blob_sha1"],
        "declared_license_path": source["declared_license_path"],
        "declared_license_url": source["declared_license_url"],
        "source_commit": source["commit"],
        "source_repo": source["repo"],
    }


def discover_source(source: dict, max_bytes: int | None) -> list[dict]:
    extensions = set(source["include_extensions"])
    rows: list[dict] = []
    tree = (
        scoped_source_tree(source)
        if source.get("tree_traversal") == "scoped"
        else source_tree(source["repo"], source["commit"])
    )
    verify_license_tree_evidence(source, tree)
    for item in tree:
        if item.get("type") != "blob":
            continue
        path = item.get("path")
        if not isinstance(path, str) or not is_safe_repo_path(path):
            continue
        extension = PurePosixPath(path).suffix.lower()
        if not path_is_scoped(path, source["path_scopes"]) or extension not in extensions:
            continue
        size = item.get("size")
        blob = item.get("sha")
        if not isinstance(size, int) or size < 0 or not isinstance(blob, str):
            raise CorpusError(f"incomplete tree metadata for {source['id']}:{path}")
        if not HEX40.fullmatch(blob):
            raise CorpusError(f"invalid Git blob ID for {source['id']}:{path}")
        tier, risks = initial_classification(source, path)
        provenance = file_provenance(source, path)
        rows.append(
            {
                "extension": extension,
                "git_blob_sha1": blob,
                "initial_rights_tier": tier,
                **source_rights_fields(source),
                **provenance,
                "risk_flags": risks,
                "rights_tier": tier,
                "source_id": source["id"],
                "source_path": path,
                "source_size": size,
                "source_url": raw_url(source["repo"], source["commit"], path),
            }
        )
    rows = sorted(rows, key=row_key)
    if "expected_files" in source and len(rows) != source["expected_files"]:
        raise CorpusError(
            f"pinned scope count mismatch for {source['id']}: "
            f"expected {source['expected_files']}, discovered {len(rows)}"
        )
    source_bytes = sum(row["source_size"] for row in rows)
    if (
        "expected_source_bytes" in source
        and source_bytes != source["expected_source_bytes"]
    ):
        raise CorpusError(
            f"pinned scope byte mismatch for {source['id']}: "
            f"expected {source['expected_source_bytes']}, discovered {source_bytes}"
        )
    if max_bytes is not None:
        rows = [row for row in rows if row["source_size"] <= max_bytes]
    return rows


def discover_all(recipe: dict, source_ids: Iterable[str], max_bytes: int | None) -> list[dict]:
    selected = set(source_ids)
    rows: list[dict] = []
    for source in recipe["sources"]:
        if source["id"] in selected:
            rows.extend(discover_source(source, max_bytes))
    return sorted(rows, key=row_key)


def row_key(row: dict) -> tuple[str, str]:
    return str(row["source_id"]), str(row["source_path"])


def row_identity(row: dict) -> str:
    return f"{row['source_id']}:{row['source_path']}"


def max_rights(left: str, right: str) -> str:
    return left if RIGHTS_ORDER[left] >= RIGHTS_ORDER[right] else right


def render_eligible(tier: str, include_tiers: set[str]) -> bool:
    """Q can be acquired for review but is never eligible for execution."""
    return tier != "Q" and tier in include_tiers


def risk_evidence(extension: str, flags: Iterable[str]) -> dict[str, bool]:
    flags = set(flags)
    return {
        "encrypted_or_invalid": bool(
            flags
            & {
                "archive_entry_limit_exceeded",
                "archive_uncompressed_limit_exceeded",
                "encrypted_zip_entry",
                "flat_xml_size_limit_exceeded",
                "invalid_flat_xml",
                "invalid_zip_package",
                "unsafe_xml_declaration",
                "unsafe_archive_member_path",
            }
        ),
        "external_link_or_connection": bool(
            flags
            & {
                "data_connection",
                "external_link_part",
                "external_relationship",
                "uninspected_relationship_metadata",
            }
        ),
        "embedded_media": "embedded_media" in flags,
        "macro_or_active_content": bool(
            flags
            & {
                "active_content_declaration",
                "active_content_extension",
                "active_content_member",
            }
        ),
        "ole_container_or_embedding": extension == ".xls" or "embedded_object" in flags,
    }


def review_decisions(tier: str, eligible: bool) -> dict[str, object]:
    if tier == "S" and eligible:
        privacy_review = "licensed_fixture_scope_and_package_metadata_reviewed"
    elif tier == "U":
        privacy_review = "restricted_pending_content_or_provenance_review"
    else:
        privacy_review = "quarantined_no_content_review"
    redistributable = tier == "S" and eligible
    return {
        "privacy_review": privacy_review,
        "render_redistributable": redistributable,
        "source_redistributable": redistributable,
    }


def relative_payload_path(row: dict) -> Path:
    path = PurePosixPath(row["source_path"])
    if not is_safe_repo_path(row["source_path"]):
        raise CorpusError(f"unsafe source path: {row['source_path']!r}")
    return Path("payload") / row["source_id"] / Path(*path.parts)


def safe_local_path(dest: Path, relative: Path) -> Path:
    if relative.is_absolute() or ".." in relative.parts:
        raise CorpusError(f"unsafe local corpus path: {relative}")
    root = dest.resolve()
    candidate = (dest / relative).resolve(strict=False)
    try:
        candidate.relative_to(root)
    except ValueError as exc:
        raise CorpusError(f"local corpus path escapes destination: {relative}") from exc
    return candidate


def matches_source_blob(row: dict, path: Path) -> bool:
    return (
        path.is_file()
        and path.stat().st_size == row["source_size"]
        and git_blob_sha1(path) == row["git_blob_sha1"]
    )


def download_one(row: dict, dest: Path, force: bool = False) -> dict:
    relative = relative_payload_path(row)
    output = safe_local_path(dest, relative)
    output.parent.mkdir(parents=True, exist_ok=True)
    if not force and matches_source_blob(row, output):
        result = dict(row)
        result.update(
            {
                "bytes": output.stat().st_size,
                "local_path": relative.as_posix(),
                "sha256": sha256_file(output),
            }
        )
        return result

    last_error: Exception | None = None
    for attempt in range(1, 4):
        temporary: Path | None = None
        try:
            request = Request(row["source_url"], headers={"User-Agent": USER_AGENT})
            content_sha = hashlib.sha1()
            content_sha.update(f"blob {row['source_size']}\0".encode("ascii"))
            file_sha = hashlib.sha256()
            total = 0
            with urlopen(request, timeout=90) as response:
                with tempfile.NamedTemporaryFile(
                    mode="wb",
                    prefix=f".{output.name}.",
                    suffix=".part",
                    dir=output.parent,
                    delete=False,
                ) as target:
                    temporary = Path(target.name)
                    while True:
                        chunk = response.read(1024 * 1024)
                        if not chunk:
                            break
                        total += len(chunk)
                        if total > row["source_size"]:
                            raise CorpusError("download exceeds the pinned source size")
                        content_sha.update(chunk)
                        file_sha.update(chunk)
                        target.write(chunk)
            if total != row["source_size"]:
                raise CorpusError(
                    f"size mismatch: expected {row['source_size']}, received {total}"
                )
            if content_sha.hexdigest() != row["git_blob_sha1"]:
                raise CorpusError("Git blob hash mismatch")
            os.replace(temporary, output)
            temporary = None
            result = dict(row)
            result.update(
                {
                    "bytes": total,
                    "local_path": relative.as_posix(),
                    "sha256": file_sha.hexdigest(),
                }
            )
            return result
        except (HTTPError, URLError, HTTPException, OSError, CorpusError) as exc:
            last_error = exc
            if temporary is not None:
                temporary.unlink(missing_ok=True)
            if attempt < 3:
                time.sleep(0.25 * attempt)
    raise CorpusError(f"failed to acquire {row_identity(row)}: {last_error}")


def bounded_member_bytes(archive: zipfile.ZipFile, info: zipfile.ZipInfo) -> bytes | None:
    if info.file_size > 4 * 1024 * 1024:
        return None
    with archive.open(info, "r") as member:
        return member.read(4 * 1024 * 1024 + 1)


def scan_package(path: Path, extension: str) -> tuple[str, list[str]]:
    if extension == ".xls":
        return "U", ["opaque_binary_container"]
    if extension == ".fods":
        return scan_flat_odf(path)
    risks: set[str] = set()
    tier = "S"
    try:
        with zipfile.ZipFile(path) as archive:
            infos = archive.infolist()
            if len(infos) > 100_000:
                return "Q", ["archive_entry_limit_exceeded"]
            if any(info.file_size > MAX_PACKAGE_MEMBER_BYTES for info in infos) or sum(
                info.file_size for info in infos
            ) > MAX_PACKAGE_UNCOMPRESSED_BYTES:
                tier = max_rights(tier, "Q")
                risks.add("archive_uncompressed_limit_exceeded")
            metadata_budget = 16 * 1024 * 1024
            for info in infos:
                normalized = info.filename.replace("\\", "/").lstrip("/").lower()
                member_path = PurePosixPath(info.filename.replace("\\", "/"))
                if member_path.is_absolute() or ".." in member_path.parts:
                    tier = max_rights(tier, "Q")
                    risks.add("unsafe_archive_member_path")
                if info.flag_bits & 0x1:
                    tier = max_rights(tier, "Q")
                    risks.add("encrypted_zip_entry")
                if normalized.startswith(("xl/vbaproject", "xl/activex/", "customui/")):
                    tier = max_rights(tier, "Q")
                    risks.add("active_content_member")
                if normalized.startswith("xl/embeddings/"):
                    tier = max_rights(tier, "Q")
                    risks.add("embedded_object")
                if normalized.startswith(("basic/", "scripts/")):
                    tier = max_rights(tier, "Q")
                    risks.add("active_content_member")
                if normalized.startswith(("xl/media/", "pictures/")):
                    tier = max_rights(tier, "U")
                    risks.add("embedded_media")
                if normalized.startswith("xl/externallinks/"):
                    tier = max_rights(tier, "U")
                    risks.add("external_link_part")
                if normalized == "xl/connections.xml":
                    tier = max_rights(tier, "U")
                    risks.add("data_connection")
                if normalized.endswith(".rels") or normalized == "[content_types].xml":
                    if info.file_size > metadata_budget:
                        payload = None
                        tier = max_rights(tier, "U")
                        risks.add("uninspected_relationship_metadata")
                    else:
                        payload = bounded_member_bytes(archive, info)
                        if payload is not None:
                            metadata_budget -= len(payload)
                    if payload is None:
                        tier = max_rights(tier, "U")
                        risks.add("uninspected_relationship_metadata")
                    elif EXTERNAL_RELATIONSHIP.search(payload):
                        tier = max_rights(tier, "U")
                        risks.add("external_relationship")
                    lower = payload.lower() if payload is not None else b""
                    if b"macroenabled" in lower or b"vbaproject" in lower:
                        tier = max_rights(tier, "Q")
                        risks.add("active_content_declaration")
    except (OSError, zipfile.BadZipFile, RuntimeError, NotImplementedError):
        return "Q", ["invalid_zip_package"]
    return tier, sorted(risks)


def scan_flat_odf(path: Path) -> tuple[str, list[str]]:
    """Classify a bounded Flat ODF workbook without resolving external data."""
    try:
        if path.stat().st_size > MAX_PACKAGE_UNCOMPRESSED_BYTES:
            return "Q", ["flat_xml_size_limit_exceeded"]
        payload = path.read_bytes()
    except OSError:
        return "Q", ["invalid_flat_xml"]
    lower = payload.lower()
    if b"<!doctype" in lower or b"<!entity" in lower:
        return "Q", ["unsafe_xml_declaration"]
    try:
        ET.fromstring(payload)
    except ET.ParseError:
        return "Q", ["invalid_flat_xml"]

    tier = "S"
    risks: set[str] = set()
    if any(
        marker in lower
        for marker in (
            b"<office:scripts",
            b"<script:event-listener",
            b"<script:script",
        )
    ):
        tier = max_rights(tier, "Q")
        risks.add("active_content_member")
    if b"<office:binary-data" in lower or b"<draw:image" in lower:
        tier = max_rights(tier, "U")
        risks.add("embedded_media")
    if re.search(
        br"xlink:href\s*=\s*['\"](?:https?|ftp|file):", lower
    ) or re.search(br"xlink:href\s*=\s*['\"]\.\.?/", lower):
        tier = max_rights(tier, "U")
        risks.add("external_relationship")
    return tier, sorted(risks)


def prepare_rows(rows: list[dict], include_tiers: set[str]) -> tuple[list[dict], list[dict]]:
    fetch: list[dict] = []
    excluded: list[dict] = []
    for row in sorted(rows, key=row_key):
        if row["initial_rights_tier"] in include_tiers:
            fetch.append(dict(row))
        else:
            item = dict(row)
            item.update(
                {
                    "eligible": False,
                    "risk_evidence": risk_evidence(item["extension"], item["risk_flags"]),
                    "status": "excluded",
                    **review_decisions(item["rights_tier"], False),
                }
            )
            excluded.append(item)
    return fetch, excluded


def finalize_rows(
    downloaded: list[dict],
    excluded: list[dict],
    dest: Path,
    include_tiers: set[str],
) -> list[dict]:
    canonical_by_sha: dict[str, dict] = {}
    finalized: list[dict] = []
    for row in sorted(downloaded, key=row_key):
        item = dict(row)
        payload_path = safe_local_path(dest, Path(item["local_path"]))
        package_tier, package_risks = scan_package(payload_path, item["extension"])
        item["risk_flags"] = sorted(set(item["risk_flags"]) | set(package_risks))
        item["rights_tier"] = max_rights(item["initial_rights_tier"], package_tier)
        item["eligible"] = render_eligible(item["rights_tier"], include_tiers)
        item["risk_evidence"] = risk_evidence(item["extension"], item["risk_flags"])
        item.update(review_decisions(item["rights_tier"], item["eligible"]))
        if item["eligible"]:
            item["semantic_sha256"] = semantic_package_sha256(
                payload_path, item["extension"]
            )
        canonical = canonical_by_sha.get(item["sha256"])
        if canonical is None:
            canonical_by_sha[item["sha256"]] = item
            item["status"] = "ready" if item["eligible"] else "quarantined"
        else:
            duplicate_path = safe_local_path(dest, Path(item["local_path"]))
            canonical_path = safe_local_path(dest, Path(canonical["local_path"]))
            if duplicate_path != canonical_path:
                duplicate_path.unlink(missing_ok=True)
            item["duplicate_of"] = row_identity(canonical)
            item["local_path"] = canonical["local_path"]
            item["status"] = "duplicate"
        finalized.append(item)
    finalized.extend(dict(row) for row in excluded)
    finalized = sorted(finalized, key=row_key)
    canonical_by_semantic: dict[str, dict] = {}
    for item in finalized:
        item["render_selected"] = False
        semantic = item.get("semantic_sha256")
        if item.get("eligible") is not True or not isinstance(semantic, str):
            continue
        canonical = canonical_by_semantic.get(semantic)
        if canonical is None:
            canonical_by_semantic[semantic] = item
            item["render_selected"] = True
        else:
            item["semantic_duplicate_of"] = row_identity(canonical)
    return finalized


def manifest_summary(rows: list[dict]) -> dict:
    by_extension: dict[str, int] = {}
    by_rights_tier: dict[str, int] = {}
    by_source: dict[str, int] = {}
    by_status: dict[str, int] = {}
    source_bytes = 0
    unique_payload_bytes = 0
    render_selected = 0
    semantic_hashes: set[str] = set()
    for row in rows:
        source_bytes += int(row["source_size"])
        for bucket, key in (
            (by_extension, row["extension"]),
            (by_rights_tier, row["rights_tier"]),
            (by_source, row["source_id"]),
            (by_status, row["status"]),
        ):
            bucket[key] = bucket.get(key, 0) + 1
        if row["status"] in {"ready", "quarantined"}:
            unique_payload_bytes += int(row["bytes"])
        if row.get("render_selected") is True:
            render_selected += 1
        if isinstance(row.get("semantic_sha256"), str):
            semantic_hashes.add(row["semantic_sha256"])
    return {
        "by_extension": dict(sorted(by_extension.items())),
        "by_rights_tier": dict(sorted(by_rights_tier.items())),
        "by_source": dict(sorted(by_source.items())),
        "by_status": dict(sorted(by_status.items())),
        "files": len(rows),
        "source_bytes": source_bytes,
        "unique_payload_bytes": unique_payload_bytes,
        "render_selected": render_selected,
        "unique_semantic_payloads": len(semantic_hashes),
    }


def recipe_manifest_path(recipe_path: Path) -> str:
    try:
        return recipe_path.resolve().relative_to(ROOT).as_posix()
    except ValueError:
        return recipe_path.name


def build_manifest(
    recipe: dict,
    recipe_path: Path,
    selected_sources: list[dict],
    rows: list[dict],
    include_tiers: set[str],
    max_bytes: int | None,
    limit: int | None,
) -> dict:
    recipe_bytes = recipe_path.read_bytes()
    return {
        "files": sorted(rows, key=row_key),
        "generated_by": "scripts/fetch-render-corpus.py",
        "rights_policy": recipe["rights_policy"],
        "rights_tiers": recipe["rights_tiers"],
        "schema": MANIFEST_SCHEMA,
        "selection": {
            "include_tiers": sorted(include_tiers),
            "limit": limit,
            "max_bytes": max_bytes,
            "source_ids": [source["id"] for source in selected_sources],
        },
        "source_recipe": {
            "path": recipe_manifest_path(recipe_path),
            "sha256": sha256_bytes(recipe_bytes),
        },
        "sources": selected_sources,
        "summary": manifest_summary(rows),
    }


def write_manifest(path: Path, manifest: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = canonical_json_bytes(manifest)
    with tempfile.NamedTemporaryFile(
        mode="wb", prefix=f".{path.name}.", suffix=".part", dir=path.parent, delete=False
    ) as target:
        temporary = Path(target.name)
        target.write(payload)
    os.replace(temporary, path)


def verify_manifest(path: Path, recipe_path: Path = DEFAULT_RECIPE) -> list[str]:
    errors: list[str] = []
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
        recipe = load_recipe(recipe_path)
    except (OSError, json.JSONDecodeError, CorpusError) as exc:
        return [str(exc)]
    if not isinstance(manifest, dict) or manifest.get("schema") != MANIFEST_SCHEMA:
        return [f"expected manifest schema {MANIFEST_SCHEMA}"]
    if manifest.get("generated_by") != "scripts/fetch-render-corpus.py":
        errors.append("manifest generated_by is not the render corpus fetcher")
    expected_recipe_sha = sha256_bytes(recipe_path.read_bytes())
    source_recipe = manifest.get("source_recipe")
    if not isinstance(source_recipe, dict):
        errors.append("manifest source_recipe must be an object")
        source_recipe = {}
    if source_recipe.get("sha256") != expected_recipe_sha:
        errors.append("source recipe SHA-256 does not match the checked-in recipe")
    if source_recipe.get("path") != recipe_manifest_path(recipe_path):
        errors.append("source recipe path is not deterministic")
    source_by_id = {source["id"]: source for source in recipe["sources"]}
    manifest_sources = manifest.get("sources")
    if not isinstance(manifest_sources, list):
        return errors + ["manifest sources must be a list"]
    manifest_source_ids = []
    invalid_source_id = False
    for source in manifest_sources:
        source_id = source.get("id") if isinstance(source, dict) else None
        if not isinstance(source_id, str):
            invalid_source_id = True
        else:
            manifest_source_ids.append(source_id)
    if invalid_source_id or manifest_source_ids != sorted(set(manifest_source_ids)):
        errors.append("manifest source ids must be sorted and unique")
    for source in manifest_sources:
        if not isinstance(source, dict) or source_by_id.get(source.get("id")) != source:
            errors.append(f"manifest source does not match recipe: {source!r}")
    selection = manifest.get("selection")
    if not isinstance(selection, dict):
        errors.append("manifest selection must be an object")
        selection = {}
    if selection.get("source_ids") != manifest_source_ids:
        errors.append("selection source_ids do not match manifest sources")
    if manifest.get("rights_policy") != recipe["rights_policy"]:
        errors.append("manifest rights policy does not match the source recipe")
    if manifest.get("rights_tiers") != recipe["rights_tiers"]:
        errors.append("manifest rights tiers do not match the source recipe")
    include_tiers = selection.get("include_tiers")
    if (
        not isinstance(include_tiers, list)
        or any(not isinstance(tier, str) for tier in include_tiers)
        or include_tiers != sorted(set(include_tiers))
    ):
        errors.append("selection include_tiers must be a sorted unique list")
        include_set: set[str] = set()
    else:
        include_set = set(include_tiers)
        if any(tier not in RIGHTS_ORDER for tier in include_set):
            errors.append("selection contains an unknown rights tier")
    rows = manifest.get("files")
    if not isinstance(rows, list):
        return errors + ["manifest files must be a list"]
    sortable_rows = all(
        isinstance(row, dict)
        and isinstance(row.get("source_id"), str)
        and isinstance(row.get("source_path"), str)
        for row in rows
    )
    if sortable_rows and rows != sorted(rows, key=row_key):
        errors.append("manifest files are not deterministically sorted")
    elif not sortable_rows:
        errors.append("manifest files contain rows without string identities")
    seen_keys: set[tuple[str, str]] = set()
    canonical_by_sha: dict[str, dict] = {}
    dest = path.parent
    for row in rows:
        if not isinstance(row, dict):
            errors.append("manifest file row is not an object")
            continue
        try:
            key = row_key(row)
            identity = row_identity(row)
        except KeyError:
            errors.append(f"manifest file row lacks identity fields: {row!r}")
            continue
        if key in seen_keys:
            errors.append(f"duplicate manifest identity: {identity}")
        seen_keys.add(key)
        source = source_by_id.get(row.get("source_id"))
        if source is None or source not in manifest_sources:
            errors.append(f"unknown source for {identity}")
            continue
        for field, expected in source_rights_fields(source).items():
            if row.get(field) != expected:
                errors.append(f"{field} mismatch for {identity}")
        source_path = row.get("source_path")
        if not isinstance(source_path, str) or not is_safe_repo_path(source_path):
            errors.append(f"unsafe source path for {identity}")
            continue
        if not path_is_scoped(source_path, source["path_scopes"]):
            errors.append(f"source path is outside the licensed scope for {identity}")
        if row.get("extension") != PurePosixPath(source_path).suffix.lower():
            errors.append(f"extension mismatch for {identity}")
        if row.get("extension") not in source["include_extensions"]:
            errors.append(f"extension is outside source policy for {identity}")
        if row.get("source_url") != raw_url(source["repo"], source["commit"], source_path):
            errors.append(f"source URL is not commit-pinned for {identity}")
        source_size = row.get("source_size")
        if not isinstance(source_size, int) or source_size < 0:
            errors.append(f"invalid source size for {identity}")
        if not HEX40.fullmatch(str(row.get("git_blob_sha1", ""))):
            errors.append(f"invalid Git blob ID for {identity}")
        initial_tier, initial_risks = initial_classification(source, source_path)
        for field, expected in file_provenance(source, source_path).items():
            if row.get(field) != expected:
                errors.append(f"{field} mismatch for {identity}")
        if row.get("initial_rights_tier") != initial_tier:
            errors.append(f"initial rights tier mismatch for {identity}")
        risk_flags = row.get("risk_flags")
        if (
            not isinstance(risk_flags, list)
            or any(not isinstance(flag, str) for flag in risk_flags)
            or risk_flags != sorted(set(risk_flags))
        ):
            errors.append(f"risk flags are not sorted and unique for {identity}")
        if row.get("rights_tier") not in RIGHTS_ORDER:
            errors.append(f"invalid rights tier for {identity}")
        status = row.get("status")
        if status == "excluded":
            if initial_tier in include_set:
                errors.append(f"included tier was marked excluded for {identity}")
            if row.get("eligible") is not False:
                errors.append(f"excluded row is eligible for {identity}")
            if any(field in row for field in ("local_path", "sha256", "bytes")):
                errors.append(f"excluded row carries payload metadata for {identity}")
            if row.get("risk_flags") != initial_risks or row.get("rights_tier") != initial_tier:
                errors.append(f"excluded row classification mismatch for {identity}")
            if row.get("risk_evidence") != risk_evidence(row["extension"], initial_risks):
                errors.append(f"excluded row risk evidence mismatch for {identity}")
            for field, expected in review_decisions(initial_tier, False).items():
                if row.get(field) != expected:
                    errors.append(f"excluded row {field} mismatch for {identity}")
            continue
        if status not in READY_STATUSES:
            errors.append(f"invalid status for {identity}: {status!r}")
            continue
        local_value = row.get("local_path")
        if not isinstance(local_value, str) or not is_safe_repo_path(local_value):
            errors.append(f"unsafe local path for {identity}")
            continue
        try:
            local = safe_local_path(dest, Path(local_value))
        except CorpusError as exc:
            errors.append(f"{identity}: {exc}")
            continue
        if not local.is_file():
            errors.append(f"payload is missing for {identity}: {local_value}")
            continue
        actual_bytes = local.stat().st_size
        actual_sha = sha256_file(local)
        actual_blob = git_blob_sha1(local)
        if row.get("bytes") != actual_bytes:
            errors.append(f"byte count mismatch for {identity}")
        if source_size != actual_bytes:
            errors.append(f"source size mismatch for {identity}")
        if not HEX64.fullmatch(str(row.get("sha256", ""))) or row.get("sha256") != actual_sha:
            errors.append(f"SHA-256 mismatch for {identity}")
        if row.get("git_blob_sha1") != actual_blob:
            errors.append(f"Git blob mismatch for {identity}")
        package_tier, package_risks = scan_package(local, row["extension"])
        expected_risks = sorted(set(initial_risks) | set(package_risks))
        expected_tier = max_rights(initial_tier, package_tier)
        expected_eligible = render_eligible(expected_tier, include_set)
        if row.get("risk_flags") != expected_risks:
            errors.append(f"risk evidence mismatch for {identity}")
        if row.get("rights_tier") != expected_tier:
            errors.append(f"rights tier mismatch for {identity}")
        if row.get("eligible") is not expected_eligible:
            errors.append(f"eligibility mismatch for {identity}")
        if row.get("risk_evidence") != risk_evidence(row["extension"], expected_risks):
            errors.append(f"risk evidence flags mismatch for {identity}")
        for field, expected in review_decisions(expected_tier, expected_eligible).items():
            if row.get(field) != expected:
                errors.append(f"{field} mismatch for {identity}")
        if expected_eligible:
            try:
                expected_semantic = semantic_package_sha256(local, row["extension"])
            except (CorpusError, OSError, zipfile.BadZipFile):
                errors.append(f"semantic package hash failed for {identity}")
            else:
                if row.get("semantic_sha256") != expected_semantic:
                    errors.append(f"semantic package hash mismatch for {identity}")
        elif "semantic_sha256" in row:
            errors.append(f"ineligible row carries semantic package hash for {identity}")
        canonical = canonical_by_sha.get(actual_sha)
        if canonical is None:
            canonical_by_sha[actual_sha] = row
            expected_status = "ready" if expected_eligible else "quarantined"
            if status != expected_status:
                errors.append(f"canonical payload has status {status!r} for {identity}")
            if "duplicate_of" in row:
                errors.append(f"canonical payload has duplicate_of for {identity}")
        else:
            if status != "duplicate":
                errors.append(f"exact-byte duplicate is not marked duplicate for {identity}")
            if row.get("duplicate_of") != row_identity(canonical):
                errors.append(f"duplicate target mismatch for {identity}")
            if row.get("local_path") != canonical.get("local_path"):
                errors.append(f"duplicate does not reuse canonical payload for {identity}")
    canonical_by_semantic: dict[str, dict] = {}
    for row in rows:
        identity = (
            row_identity(row)
            if isinstance(row, dict)
            and isinstance(row.get("source_id"), str)
            and isinstance(row.get("source_path"), str)
            else "malformed-row"
        )
        semantic = row.get("semantic_sha256") if isinstance(row, dict) else None
        eligible = isinstance(row, dict) and row.get("eligible") is True
        if not eligible or not isinstance(semantic, str):
            if isinstance(row, dict) and row.get("render_selected") is not False:
                errors.append(f"ineligible row is render-selected for {identity}")
            if isinstance(row, dict) and "semantic_duplicate_of" in row:
                errors.append(f"ineligible row has semantic duplicate for {identity}")
            continue
        canonical = canonical_by_semantic.get(semantic)
        if canonical is None:
            canonical_by_semantic[semantic] = row
            if row.get("render_selected") is not True:
                errors.append(f"semantic canonical is not render-selected for {identity}")
            if "semantic_duplicate_of" in row:
                errors.append(f"semantic canonical has duplicate target for {identity}")
        else:
            if row.get("render_selected") is not False:
                errors.append(f"semantic duplicate is render-selected for {identity}")
            if row.get("semantic_duplicate_of") != row_identity(canonical):
                errors.append(f"semantic duplicate target mismatch for {identity}")
    try:
        expected_summary = manifest_summary(rows)
    except (KeyError, TypeError, ValueError):
        errors.append("manifest summary cannot be derived from malformed file rows")
    else:
        if manifest.get("summary") != expected_summary:
            errors.append("manifest summary does not match its file rows")
    return errors


def compact_summary(rows: list[dict]) -> str:
    summary = manifest_summary(rows)
    groups = []
    for name in ("by_source", "by_extension", "by_rights_tier", "by_status"):
        value = summary[name]
        groups.append(name + "=" + ",".join(f"{key}:{value[key]}" for key in sorted(value)))
    return (
        f"files={summary['files']} source_bytes={summary['source_bytes']} "
        f"unique_payload_bytes={summary['unique_payload_bytes']} "
        f"render_selected={summary['render_selected']} "
        f"unique_semantic_payloads={summary['unique_semantic_payloads']} "
        + " ".join(groups)
    )


def resolve_dest(path: Path) -> Path:
    return path.resolve() if path.is_absolute() else (ROOT / path).resolve()


def print_sources(sources: list[dict]) -> None:
    print(
        "source_id\tcommit\trights\tlicense\texpected_files\t"
        "expected_source_bytes\tpath_scope\tlicense_url\tlicense_blob_sha1\t"
        "license_path\tsource_url\tattribution"
    )
    for source in sources:
        print(
            "\t".join(
                [
                    source["id"],
                    source["commit"],
                    source["rights_tier"],
                    source["declared_license"],
                    str(source["expected_files"]),
                    str(source["expected_source_bytes"]),
                    ",".join(source["path_scopes"]),
                    source["declared_license_url"],
                    source["declared_license_blob_sha1"],
                    source["declared_license_path"],
                    source["source_url"],
                    source["attribution"],
                ]
            )
        )


def print_rows(rows: list[dict]) -> None:
    print("source_id\trights\tsize\tgit_blob_sha1\tsource_path\tsource_url")
    for row in rows:
        print(
            "\t".join(
                [
                    row["source_id"],
                    row["initial_rights_tier"],
                    str(row["source_size"]),
                    row["git_blob_sha1"],
                    row["source_path"],
                    row["source_url"],
                ]
            )
        )


def select_sources(recipe: dict, requested: list[str] | None) -> list[dict]:
    source_by_id = {source["id"]: source for source in recipe["sources"]}
    ids = requested or sorted(source_by_id)
    unknown = sorted(set(ids) - set(source_by_id))
    if unknown:
        raise CorpusError(f"unknown sources: {unknown}")
    return [source_by_id[source_id] for source_id in sorted(set(ids))]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_mutually_exclusive_group()
    actions.add_argument(
        "--list-sources",
        action="store_true",
        help="list the pinned policy without network access",
    )
    actions.add_argument(
        "--list",
        action="store_true",
        help="list discovered files without writing payloads",
    )
    actions.add_argument(
        "--dry-run",
        action="store_true",
        help="discover and summarize without writing payloads",
    )
    actions.add_argument(
        "--verify",
        action="store_true",
        help="verify an existing local manifest and every payload",
    )
    parser.add_argument("--recipe", type=Path, default=DEFAULT_RECIPE)
    parser.add_argument("--dest", type=Path, default=DEFAULT_DEST)
    parser.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help="manifest path for --verify; defaults to DEST/manifest.json",
    )
    parser.add_argument(
        "--source",
        action="append",
        help="source id; repeatable; defaults to all pinned sources",
    )
    parser.add_argument(
        "--include-tier",
        action="append",
        choices=sorted(RIGHTS_ORDER),
        help="rights tier to acquire; repeatable; defaults to the recipe's S-only policy",
    )
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args(argv)

    try:
        recipe = load_recipe(args.recipe)
        sources = select_sources(recipe, args.source)
        include_tiers = set(args.include_tier or recipe["default_include_tiers"])
        if args.max_bytes < 0:
            raise CorpusError("--max-bytes must be non-negative")
        if args.limit is not None and args.limit < 0:
            raise CorpusError("--limit must be non-negative")
        if args.jobs < 1:
            raise CorpusError("--jobs must be positive")
        if args.manifest is not None and not args.verify:
            raise CorpusError("--manifest is only valid with --verify")
        if args.list_sources:
            print_sources(sources)
            return 0
        if args.verify:
            manifest_path = args.manifest or resolve_dest(args.dest) / "manifest.json"
            errors = verify_manifest(manifest_path, args.recipe)
            if errors:
                for error in errors:
                    print(f"ERROR {error}", file=sys.stderr)
                return 1
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            print("verified:", compact_summary(manifest["files"]))
            return 0

        rows = discover_all(recipe, [source["id"] for source in sources], args.max_bytes)
        if args.limit is not None:
            rows = rows[: args.limit]
        if args.list:
            print_rows(rows)
            return 0
        if args.dry_run:
            preview = []
            for row in rows:
                item = dict(row)
                item.update(
                    {
                        "eligible": render_eligible(
                            row["initial_rights_tier"], include_tiers
                        ),
                        "status": (
                            "quarantine_candidate"
                            if row["initial_rights_tier"] == "Q"
                            and row["initial_rights_tier"] in include_tiers
                            else (
                                "candidate"
                                if row["initial_rights_tier"] in include_tiers
                                else "excluded"
                            )
                        ),
                    }
                )
                preview.append(item)
            print("dry-run:", compact_summary(preview))
            return 0

        fetch, excluded = prepare_rows(rows, include_tiers)
        dest = resolve_dest(args.dest)
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            downloaded = list(pool.map(lambda row: download_one(row, dest, args.force), fetch))
        finalized = finalize_rows(downloaded, excluded, dest, include_tiers)
        manifest = build_manifest(
            recipe,
            args.recipe,
            sources,
            finalized,
            include_tiers,
            args.max_bytes,
            args.limit,
        )
        manifest_path = dest / "manifest.json"
        write_manifest(manifest_path, manifest)
        errors = verify_manifest(manifest_path, args.recipe)
        if errors:
            raise CorpusError("generated manifest failed verification: " + "; ".join(errors))
        print("acquired:", compact_summary(finalized))
        print(f"manifest: {manifest_path}")
        return 0
    except (CorpusError, OSError) as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
